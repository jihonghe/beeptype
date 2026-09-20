import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:connectivity_plus/connectivity_plus.dart';
import 'dart:async';
import '../services/lan_check.dart';
import '../services/retry_policy.dart';
import '../services/text_sender.dart';
import '../services/tcp_service.dart';

class HomePage extends StatefulWidget {
  final TextSender sender;

  const HomePage({super.key, required this.sender});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> with WidgetsBindingObserver {
  /// 语音松手提交后的发送延时（毫秒），当前为 0 = 松手即发。
  ///
  /// 触发原理：微信输入法按住说话期间 composing 始终有效（换气停顿也有效），
  /// 松手"最终提交"时 composing 变 OFF——下降沿本身就是松手事件，不需要
  /// 等待静音停顿。实测微信最终提交时文本已就位、只清 composing 标记，
  /// 故 0ms 安全：Timer(0) 仍让出当前调用栈，合并同批次内的后续通知。
  /// 若将来发现某输入法"先清 composing、下一帧才写最终文本"，再回调到 50~100ms。
  ///
  /// 不适配：搜狗类"句末停顿先提交一句、随后再追加新一段"的输入法。
  /// 其句间间隔实测 422~1689ms，只能靠秒级观察窗区分停顿与说完，
  /// 与"松手即发"不可兼得。
  static const int _voiceCommitDelayMs = 0;


  late TextEditingController _inputController;
  late FocusNode _focusNode;
  late StreamSubscription<ConnectionStatus> _statusSub;
  late StreamSubscription<String> _sessionEndedSub;
  late StreamSubscription<void> _sessionUpdatedSub;
  Timer? _debounce;
  Timer? _reconnectTimer;
  ConnectionStatus _status = ConnectionStatus.disconnected;
  bool _sending = false;
  bool _hasText = false;

  /// 是否正处在一次主动连接尝试中（合并事件回调与收尾逻辑，避免双重排程）
  bool _connecting = false;
  int _reconnectAttempt = 0;

  /// 最近一次失败"看起来不在同一局域网"（决定打字区要不要提示检查 Wi-Fi）
  bool _offLan = false;

  /// 已停止自动重试（快速试满一轮，或根本不在局域网上）——零定时器零广播，
  /// 直到切回前台 / 网络变化 / 用户点「重新连接」才会重新开始
  bool _retryStopped = false;

  /// 当前占用态是不是"扫码"撞上的：只有凭凭证回来的常用设备才配显示接管按钮
  bool _occupiedViaScan = false;

  StreamSubscription<List<ConnectivityResult>>? _netSub;

  // 上一次回调是否处于 composing（组合态），用于检测"有效→无效"的下降沿
  bool _wasComposing = false;
  // 上一次回调的文本，用于在非 composing 模式下区分真正的文本变化
  String _lastText = '';

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    _inputController = TextEditingController();
    _focusNode = FocusNode();
    _inputController.addListener(_onValueChanged);
    _statusSub = widget.sender.tcp.statusStream.listen(_onStatus);
    _sessionEndedSub =
        widget.sender.tcp.sessionEndedStream.listen(_onSessionEnded);
    // 电脑端可能随时把本机设为常用设备（身份变化），刷新界面
    _sessionUpdatedSub = widget.sender.sessionUpdated.listen((_) {
      if (mounted) setState(() {});
    });
    // 网络一变（换 Wi-Fi / 切回原网络 / 开关数据）立刻重试，不干等退避周期
    _netSub = Connectivity().onConnectivityChanged.listen((_) {
      if (!mounted || _connecting) return;
      if (_status == ConnectionStatus.paired) return;
      if (!_canAutoConnect()) return;
      _reconnectTimer?.cancel();
      _reconnectAttempt = 0; // 环境变了：重新开始一轮快速重试
      unawaited(_attemptConnect());
    });

    // 冷启动：有绑定电脑且不是"本进程已手动断开"状态时自动连接
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _attemptConnect();
    });
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _statusSub.cancel();
    _sessionEndedSub.cancel();
    _sessionUpdatedSub.cancel();
    _netSub?.cancel();
    _debounce?.cancel();
    _reconnectTimer?.cancel();
    _inputController.dispose();
    _focusNode.dispose();
    super.dispose();
  }

  // ----------------------------------------------------------------------
  // 连接生命周期：冷启自动连、断线退避自动连、切回前台立即试
  //
  // 规则：常用设备/临时切后台行为一致——不主动断开，5s 心跳继续，断线同样
  // 按退避重连（Doze 下 Timer 会被系统延迟，切回前台立即补一次）。
  // ----------------------------------------------------------------------

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    // 切走方向（resumed→inactive）主动收起键盘，消除转场快照导致的键盘闪动
    final bool leavingForeground =
        state == AppLifecycleState.inactive &&
            _lastLifecycleState == AppLifecycleState.resumed;
    _lastLifecycleState = state;
    if (leavingForeground && _status == ConnectionStatus.paired) {
      unawaited(SystemChannels.textInput.invokeMethod<void>('TextInput.hide'));
    }

    if (state == AppLifecycleState.resumed) {
      _onResumed();
    }
    // 进入后台：不取消重连排程、不停心跳——切后台不是断开理由，
    // 常用设备/临时行为一致。Doze 下 Timer 被延迟，切回前台在 _onResumed 立即补试。
  }

  AppLifecycleState? _lastLifecycleState;

  void _onResumed() {
    // 用户可能在系统设置里改了设备名：顺手同步（毫秒级），下次连接 PC 即显示新名
    unawaited(widget.sender.refreshDeviceName());
    if (_status == ConnectionStatus.paired) {
      // 连接还活着：无条件重新聚焦+唤起键盘
      unawaited(_requestKeyboard());
      return;
    }
    if (!_canAutoConnect()) return;
    if (_connecting ||
        _status == ConnectionStatus.connecting ||
        _status == ConnectionStatus.pairing) {
      return;
    }
    _reconnectTimer?.cancel();
    _reconnectAttempt = 0;
    _attemptConnect();
  }

  // 有可自动连接的目标：已绑定电脑，或"窗口期内"的临时会话（断线续连）
  bool _canAutoConnect() =>
      widget.sender.hasAnyTarget && !widget.sender.manualDisconnected;

  void _onStatus(ConnectionStatus status) {
    if (!mounted) return;
    setState(() => _status = status);

    switch (status) {
      case ConnectionStatus.paired:
        _connecting = false;
        _reconnectTimer?.cancel();
        _reconnectAttempt = 0;
        unawaited(_requestKeyboard());
        break;
      case ConnectionStatus.occupied:
        _connecting = false;
        _reconnectTimer?.cancel();
        _reconnectAttempt = 0;
        break;
      case ConnectionStatus.disconnected:
        // 主动尝试中的连接失败由 _attemptConnect 收尾统一排程；
        // 这里处理"使用中突发断线"（WiFi 抖动、PC 退出、被踢等）。
        if (!_connecting && _canAutoConnect()) {
          _scheduleReconnect();
        }
        break;
      default:
        break;
    }
  }

  /// 服务端主动推送的会话事件（PC 托盘断开、解绑降级、常用设备接管、同身份替换）
  void _onSessionEnded(String reason) {
    switch (reason) {
      case 'owner_disconnect':
        // 电脑端主动断开 = 明确的用户意图，本进程不再自动重连
        widget.sender.markDisconnectedByPc();
        _reconnectTimer?.cancel();
        _showSnack('电脑端已断开连接');
        break;
      case 'downgraded':
        // 电脑端解绑本机：连接降级为临时（会话保持），静默删除本地凭证。
        // 不弹提示：解绑是用户在电脑上的主动操作，胶囊出现"临时设备"灰标即是反馈
        unawaited(widget.sender.downgradeToTemp().then((_) {
          if (mounted) setState(() {});
        }));
        break;
      case 'owner_takeover':
        // 自己是临时方：该机的常用设备接管，中性提示
        _showSnack('这台电脑的常用设备已接管');
        break;
      case 'replaced':
      default:
        // 同身份新连接替换旧连接等内部情形：静默，按普通断线处理
        break;
    }
  }

  /// 排程一次退避自动重连（前后台一致）。
  ///
  /// [net] 为本机网络快照：**只有移动数据时不重试**——本产品只在同一局域网下能工作，
  /// 连上去也无从谈起，反复重试与广播只是白耗电；Wi-Fi 回来时由网络变化监听重新拉起。
  /// 传 null 表示"还没来得及判断"（突发断线路径），按可能有局域网处理。
  void _scheduleReconnect([LocalNet? net]) {
    if (!_canAutoConnect()) return;
    if (_status == ConnectionStatus.paired ||
        _status == ConnectionStatus.occupied ||
        _connecting) {
      return;
    }

    _reconnectTimer?.cancel();

    // 只有移动数据（枚举不到任何局域网网卡）→ 停手，不做无意义的尝试
    if (net != null && !net.hasLan) {
      _retryStopped = true;
      return;
    }
    // 快速试满一轮就停手：慢速重试的价值极低，不如让用户当场点「重新连接」
    if (!retryAllowed(_reconnectAttempt)) {
      _retryStopped = true;
      return;
    }

    final delayMs = retryDelayMs(_reconnectAttempt);
    _reconnectAttempt++;
    _retryStopped = false;
    _reconnectTimer =
        Timer(Duration(milliseconds: delayMs), _attemptConnect);
  }

  /// 尝试连接最近使用的常用电脑（缓存 IP 直连 + UDP 发现并行，见 TextSender）
  Future<void> _attemptConnect() async {
    _reconnectTimer?.cancel();
    if (_connecting) return;
    if (!_canAutoConnect()) return;
    if (_status == ConnectionStatus.paired ||
        _status == ConnectionStatus.occupied) {
      return;
    }

    _connecting = true;
    _occupiedViaScan = false; // 自动连接走的是凭证路径，接管按钮才有效
    _retryStopped = false; // 重新开始一轮快速重试
    HelloOutcome? outcome;
    try {
      outcome = await widget.sender.autoConnect();
    } catch (_) {}
    if (!mounted) return;

    // 临时会话的续连窗口过期（或被 PC 作废）时给出明确出口，而不是静默回到引导页
    final expiredName = widget.sender.consumeTransientExpiredNotice();
    if (expiredName != null) {
      _showSnack('临时会话已过期，请重新扫码');
    }

    // 电脑**明确拒绝**本机凭证（典型：电脑端解除了与这台设备的绑定）：
    // 凭证已失效 → 删除并回扫码页，并且要说明原因（否则用户莫名其妙）
    if (outcome != null && outcome.isUnauthorized) {
      final id = widget.sender.storage.primaryDevice?.deviceId;
      await widget.sender.forgetDevice(id);
      _connecting = false;
      if (mounted) {
        _showSnack('电脑已解除与你的绑定，请重新扫码');
        setState(() {});
      }
      return;
    }

    // 连不上：枚举一次本机网络，同时回答两个问题——
    // ① 是不是根本不在同一个局域网（能判断才提示，判断不了就沉默）；
    // ② 还有没有局域网可试（只有移动数据就不必再折腾了）
    final net = await localNetSnapshot();
    final target = widget.sender.lastAttemptTargetIp;
    _connecting = false;
    if (!mounted) return;
    _offLan = target != null && likelyOffLan(target, net?.ips ?? const []);
    _scheduleReconnect(net);
    setState(() {});
  }

  /// 手动点灰钮连接：清除手动断开标记，立即尝试
  Future<void> _manualConnect() async {
    widget.sender.manualDisconnected = false;
    _reconnectTimer?.cancel();
    _reconnectAttempt = 0;
    await _attemptConnect();
  }

  /// 点绿灯断开：bye 优雅告别，本进程不再自动重连（再点灰钮才连）
  Future<void> _manualDisconnect() async {
    await widget.sender.userDisconnect();
    if (mounted) setState(() {});
  }

  /// occupied 状态下接管电脑
  Future<void> _takeover() async {
    final ok = await widget.sender.takeover();
    if (!mounted) return;
    if (!ok) _showSnack('接管失败，请重试');
  }

  /// 已配对时自动聚焦输入框并强制唤起软键盘。
  ///
  /// 冷启动竞态：自动连接在 Activity 窗口完成 IME 注册前就绪时，
  /// TextInput.show 会被系统取消（ImeTracker: PHASE_CLIENT_ALREADY_HIDDEN，
  /// HyperOS 实测复现）。因此 show 后验证键盘是否真的弹出（viewInsets），
  /// 未弹则以 350ms 间隔重试，最多 3 次；切回前台等窗口就绪的场景首次即成功。
  Future<void> _requestKeyboard() async {
    if (_status != ConnectionStatus.paired) return;
    for (var attempt = 0; attempt < 3; attempt++) {
      // 键盘已经在显示就不重复召唤，避免多余的收起/弹起闪动
      final views = WidgetsBinding.instance.platformDispatcher.views;
      if (views.isNotEmpty && views.first.viewInsets.bottom > 0) return;
      await WidgetsBinding.instance.endOfFrame;
      if (!mounted || _status != ConnectionStatus.paired) return;
      _focusNode.requestFocus();
      await SystemChannels.textInput.invokeMethod<void>('TextInput.show');
      await Future<void>.delayed(const Duration(milliseconds: 350));
    }
  }

  // ----------------------------------------------------------------------
  // 输入监听：composing 下降沿松手即发（逻辑与上一版一致）
  // ----------------------------------------------------------------------

  void _onValueChanged() {
    final v = _inputController.value;
    final text = v.text;
    final composing = v.composing;
    final isComposing = composing.isValid && composing.start != composing.end;

    if (text.isNotEmpty != _hasText) {
      setState(() => _hasText = text.isNotEmpty);
    }

    if (text.isEmpty) {
      _debounce?.cancel();
      _wasComposing = false;
      _lastText = '';
      return;
    }

    // 未持席时只观察、不发送
    if (_status != ConnectionStatus.paired) {
      _lastText = text;
      _wasComposing = isComposing;
      return;
    }

    if (isComposing) {
      _debounce?.cancel();
      _wasComposing = true;
    } else if (_wasComposing) {
      _wasComposing = false;
      _scheduleVoiceCommitSend();
    } else if (text != _lastText) {
      _scheduleSend(text);
    }

    _lastText = text;
  }

  /// 语音松手提交排程：延时 0ms 发送输入框【当前全文】
  void _scheduleVoiceCommitSend() {
    _debounce?.cancel();
    _debounce = Timer(const Duration(milliseconds: _voiceCommitDelayMs), () {
      _sendText(_inputController.value.text);
    });
  }

  /// 防抖排程发送：300ms 内无新变化则发送快照文本（普通键盘输入）
  void _scheduleSend(String text) {
    _debounce?.cancel();
    _debounce = Timer(const Duration(milliseconds: 300), () {
      _sendText(text);
    });
  }

  Future<void> _sendText(String text) async {
    if (_sending) return;
    final trimmed = text.trim();
    if (trimmed.isEmpty) return;
    setState(() => _sending = true);

    try {
      final ack = await widget.sender.sendText(text);
      if (ack.ok) {
        _inputController.clear();
      } else {
        _showSnack('发送失败（${ack.code ?? '未知'}），文字已保留，点右侧按钮重试');
      }
    } catch (e) {
      _showSnack('发送异常：$e');
    } finally {
      if (mounted) setState(() => _sending = false);
    }
  }

  void _showSnack(String message) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(
        content: Text(message),
        duration: const Duration(seconds: 3),
      ));
  }

  // ----------------------------------------------------------------------
  // UI：输入区为主角，连接状态降级为顶部胶囊；页面骨架不随状态跳变
  // ----------------------------------------------------------------------

  /// 顶部状态胶囊：单行定高。状态点 + 设备名 + 设备名右侧灰色小标签 + 操作。
  ///
  /// 防跳动：胶囊整体固定 56 高（M3 TextButton 默认 48 触控高会把
  /// 无按钮状态的胶囊"矮一截"，故按钮 shrinkWrap + 外层定高）；
  /// 设备名永远垂直居中；状态只用点颜色与右侧标签表达，不增删行。
  Widget _buildStatusPill(ColorScheme scheme) {
    final session = widget.sender.session;
    // 设备名在连接/断开各状态下保持原位，断开不换标题，只用灰点表达
    final knownName = session?.deviceName ??
        widget.sender.storage.primaryDevice?.name ??
        widget.sender.storage.transientSession?.name;

    Widget leading;
    late String title;
    String? tag;
    // 中性提示标签压暗到约 60%：它是辅助说明，不与设备名同级
    Color tagColor = scheme.onSurfaceVariant.withValues(alpha: 0.6);
    Widget? trailing;

    // 胶囊内文字按钮：收缩触控热区到视觉尺寸，高度不超过定高胶囊
    ButtonStyle pillButtonStyle(Color? color) => TextButton.styleFrom(
          foregroundColor: color,
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          minimumSize: const Size(0, 36),
          padding: const EdgeInsets.symmetric(horizontal: 10),
          visualDensity: VisualDensity.compact,
        );

    switch (_status) {
      case ConnectionStatus.paired:
        final isPrimary = session?.isPrimary == true;
        leading = _StatusDot(Colors.green.shade600);
        title = knownName ?? '电脑';
        // 常用设备是默认态，不标注；临时设备在名字右侧给灰字小标签
        if (!isPrimary) tag = '临时设备';
        // "断开"是低频操作：明确置灰，不与主色行动争抢
        trailing = TextButton(
          onPressed: _manualDisconnect,
          style: pillButtonStyle(Colors.grey.shade600),
          child: const Text('断开'),
        );
      case ConnectionStatus.occupied:
        leading = _StatusDot(Colors.orange.shade700);
        title = knownName ?? '电脑';
        tag = '另一台手机正在使用';
        // 警示态保留橙色但略压，不喧宾夺主
        tagColor = Colors.orange.shade800.withValues(alpha: 0.85);
      case ConnectionStatus.connecting:
      case ConnectionStatus.pairing:
      case ConnectionStatus.connected:
        leading = const SizedBox(
          width: 14,
          height: 14,
          child: CircularProgressIndicator(strokeWidth: 2),
        );
        title = knownName ?? '电脑';
        tag = '正在连接…';
      case ConnectionStatus.disconnected:
      case ConnectionStatus.failed:
        leading = _StatusDot(Colors.grey.shade400);
        if (!widget.sender.hasDevices) {
          // 未绑定（含临时/降级会话断开后）：回到引导态，正文即扫码入口
          title = '未连接电脑';
        } else {
          title = knownName ?? '电脑';
          final stopped = _retryStopped || widget.sender.manualDisconnected;
          tag = stopped ? '已停止重试' : '连接失败，重试中…';
          trailing = TextButton(
            onPressed: _manualConnect,
            style: pillButtonStyle(null),
            child: Text(stopped ? '重新连接' : '立即重试'),
          );
        }
    }

    return Card(
      margin: EdgeInsets.zero,
      elevation: 0,
      color: scheme.surfaceContainerHighest,
      shape:
          RoundedRectangleBorder(borderRadius: BorderRadius.circular(16)),
      child: SizedBox(
        height: 56,
        child: Padding(
          padding: const EdgeInsets.only(left: 14, right: 6),
          child: Row(
            children: [
              // leading 统一 14 盒：10px 圆点居中其中，与 14px 转圈等宽
              SizedBox(
                width: 14,
                height: 14,
                child: Center(child: leading),
              ),
              const SizedBox(width: 12),
              // 设备名+标签整体占满中间剩余空间，操作按钮因此永远贴右
              Expanded(
                child: Row(
                  children: [
                    Flexible(
                      child: Text(
                        title,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                            fontSize: 15, fontWeight: FontWeight.w600),
                      ),
                    ),
                    if (tag != null) ...[
                      const SizedBox(width: 8),
                      Flexible(
                        child: Text(
                          tag,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(fontSize: 12, color: tagColor),
                        ),
                      ),
                    ],
                  ],
                ),
              ),
              if (trailing != null) trailing,
            ],
          ),
        ),
      ),
    );
  }

  /// 主体输入卡：已连接可输入，其余状态同卡原位禁用，骨架不跳变。
  Widget _buildEditorCard(ColorScheme scheme, {required bool enabled}) {
    // 打字区只讲"该做什么"（状态由胶囊区讲，不复述）：默认一句功能说明，
    // 能判断"不在同一个局域网"时换成那条唯一有用的建议
    final String hintText;
    if (enabled) {
      hintText = '长按键盘语音键即可语音输入文字';
    } else if (_offLan) {
      hintText = '请确认手机和电脑连着同一个 Wi-Fi';
    } else {
      hintText = '连接成功后即可语音输入文字';
    }

    return Card(
      margin: EdgeInsets.zero,
      elevation: 0,
      // 背景/边框不随连接状态切换，避免断开瞬间卡片"变色跳一下"
      color: scheme.surface,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(20),
        side: BorderSide(color: scheme.outlineVariant),
      ),
      child: Padding(
        padding: const EdgeInsets.fromLTRB(16, 6, 8, 6),
        child: Column(
          children: [
            Expanded(
              child: TextField(
                controller: _inputController,
                focusNode: _focusNode,
                enabled: enabled,
                expands: true,
                maxLines: null,
                minLines: null,
                textAlignVertical: TextAlignVertical.top,
                style: TextStyle(
                  fontSize: 18,
                  height: 1.4,
                  color: scheme.onSurface,
                ),
                decoration: InputDecoration(
                  hintText: hintText,
                  hintMaxLines: 3,
                  // 引导文案刻意压暗：它是提示不是内容，不与真实输入争注意力
                  hintStyle: TextStyle(
                    fontSize: 18,
                    height: 1.4,
                    color: scheme.onSurfaceVariant
                        .withValues(alpha: 0.5),
                  ),
                  border: InputBorder.none,
                  focusedBorder: InputBorder.none,
                  enabledBorder: InputBorder.none,
                  disabledBorder: InputBorder.none,
                ),
              ),
            ),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                if (_sending)
                  const Padding(
                    padding: EdgeInsets.all(10),
                    child: SizedBox(
                      width: 22,
                      height: 22,
                      child: CircularProgressIndicator(strokeWidth: 2.5),
                    ),
                  )
                else if (enabled && _hasText)
                  IconButton.filled(
                    onPressed: () =>
                        _sendText(_inputController.value.text),
                    tooltip: '发送（正常松手即自动发送，此处为兜底）',
                    icon: const Icon(Icons.send_rounded, size: 20),
                  ),
              ],
            ),
          ],
        ),
      ),
    );
  }

  /// occupied：电脑正被另一台手机占用，接管操作只在此处显式提供。
  Widget _buildOccupiedView(ColorScheme scheme) {
    return Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.symmetric(vertical: 24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: 104,
              height: 104,
              decoration: BoxDecoration(
                color: Colors.orange.shade50,
                shape: BoxShape.circle,
              ),
              child: Icon(Icons.devices_other_rounded,
                  size: 48, color: Colors.orange.shade700),
            ),
            const SizedBox(height: 24),
            Text('电脑使用中',
                style: TextStyle(
                    fontSize: 20,
                    fontWeight: FontWeight.w700,
                    color: Colors.orange.shade800)),
            const SizedBox(height: 8),
            Text('另一台手机正在使用这台电脑',
                style: TextStyle(
                    fontSize: 14, color: scheme.onSurfaceVariant)),
            const SizedBox(height: 28),
            // 接管只对"凭长期凭证回来的常用设备"有效（电脑端只放行这种连接）。
            // 扫码撞上占用的人点了必然失败，所以干脆不给按钮，只留上面那句说明。
            if (!_occupiedViaScan) ...[
              FilledButton.icon(
                onPressed: _takeover,
                style: FilledButton.styleFrom(
                  backgroundColor: Colors.orange.shade700,
                  minimumSize: const Size.fromHeight(48),
                  shape: RoundedRectangleBorder(
                      borderRadius: BorderRadius.circular(14)),
                ),
                icon: const Icon(Icons.login_rounded),
                label: const Text('接管电脑'),
              ),
              const SizedBox(height: 12),
              Text('接管后对方将收到提示；不操作则保持等待',
                  style: TextStyle(
                      fontSize: 12, color: scheme.onSurfaceVariant)),
            ] else
              Text('请在电脑上点「断开」腾出席位后再扫码',
                  textAlign: TextAlign.center,
                  style: TextStyle(
                      fontSize: 12, color: scheme.onSurfaceVariant)),
          ],
        ),
      ),
    );
  }

  /// 未绑定任何电脑（含临时/降级会话断开后）的引导页：唯一动作是扫码绑定
  Widget _buildOnboardingView(ColorScheme scheme) {
    return Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.symmetric(vertical: 24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: 104,
              height: 104,
              decoration: BoxDecoration(
                color: scheme.primaryContainer,
                shape: BoxShape.circle,
              ),
              child: Icon(Icons.qr_code_scanner_rounded,
                  size: 48, color: scheme.primary),
            ),
            const SizedBox(height: 24),
            const Text('扫码连接电脑',
                style: TextStyle(
                    fontSize: 20, fontWeight: FontWeight.w700)),
            const SizedBox(height: 8),
            Text('连接后即可语音输入文字',
                style:
                    TextStyle(fontSize: 14, color: scheme.onSurfaceVariant)),
            const SizedBox(height: 28),
            FilledButton.icon(
              onPressed: _navigateToScan,
              style: FilledButton.styleFrom(
                minimumSize: const Size.fromHeight(48),
                shape: RoundedRectangleBorder(
                    borderRadius: BorderRadius.circular(14)),
              ),
              icon: const Icon(Icons.qr_code_scanner_rounded),
              label: const Text('扫码连接电脑'),
            ),
            const SizedBox(height: 12),
            Text('在电脑端打开 BeeType，用本机扫描其二维码',
                style:
                    TextStyle(fontSize: 12, color: scheme.onSurfaceVariant)),
          ],
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final paired = _status == ConnectionStatus.paired;
    final occupied = _status == ConnectionStatus.occupied;
    final hasTarget = widget.sender.hasAnyTarget;
    final busy = _status == ConnectionStatus.connecting ||
        _status == ConnectionStatus.pairing ||
        _status == ConnectionStatus.connected;

    return Scaffold(
      appBar: AppBar(
        title: const Text('BeeType'),
        actions: [
          IconButton(
            icon: const Icon(Icons.qr_code_scanner_rounded),
            tooltip: '扫码连接电脑',
            onPressed: _navigateToScan,
          ),
        ],
      ),
      body: SafeArea(
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 12),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              _buildStatusPill(scheme),
              const SizedBox(height: 14),
              Expanded(
                child: occupied
                    ? _buildOccupiedView(scheme)
                    : paired
                        ? _buildEditorCard(scheme, enabled: true)
                        : (!hasTarget && !busy)
                            ? _buildOnboardingView(scheme)
                            : _buildEditorCard(scheme, enabled: false),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Future<void> _navigateToScan() async {
    // 记下来源：扫码撞上"电脑使用中"时不该给接管按钮（那按钮点了必然失败）
    _occupiedViaScan = true;
    final result = await Navigator.of(context).pushNamed('/scan');
    if (result == true && mounted) {
      setState(() {});
    }
  }
}

/// 状态胶囊左侧的小圆点（绿=已连接，橙=占用，灰=未连接）
class _StatusDot extends StatelessWidget {
  final Color color;

  const _StatusDot(this.color);

  @override
  Widget build(BuildContext context) {
    return Container(
      width: 10,
      height: 10,
      decoration: BoxDecoration(color: color, shape: BoxShape.circle),
    );
  }
}
