import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:connectivity_plus/connectivity_plus.dart';
import 'dart:async';
import '../services/lan_check.dart';
import '../services/retry_episode.dart';
import '../services/retry_policy.dart';
import '../services/route_observer.dart';
import '../services/text_sender.dart';
import '../services/tcp_service.dart';

class HomePage extends StatefulWidget {
  final TextSender sender;

  const HomePage({super.key, required this.sender});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage>
    with WidgetsBindingObserver, RouteAware {
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



  /// 最近一次失败"看起来不在同一局域网"（决定打字区要不要提示检查 Wi-Fi）
  bool _offLan = false;

  /// 最近一次断开是否由"对端关闭连接"造成（电脑上程序退出/重启）——只用于判定归属
  bool _peerClosed = false;

  /// 本次连接是否已经收过 session_ended（被踢/主人接管）：那种情况另有提示，
  /// 不再按"对端关闭"重复处理一遍
  bool _sessionEndedHandled = false;

  /// 临时记录到期时的一次性刷新：让界面自己跳回扫码引导页
  /// （手机端是事件驱动的，没有这一步会停在输入卡上直到用户碰它）
  Timer? _transientExpiryTimer;

  /// 最近一次网络变化（换 Wi-Fi / 切回原网络 / 开关移动数据）的时刻：
  /// 用来判断"对端关闭连接"是不是我们自己换网络造成的
  DateTime? _lastNetChangeAt;

  /// 手机自己的网络刚刚（15 秒内）变化过
  bool get _netChangedRecently {
    final t = _lastNetChangeAt;
    return t != null && DateTime.now().difference(t) < const Duration(seconds: 15);
  }

  /// "一轮快速重试"的状态机：显式状态 + 明确的触发，见 retry_episode.dart
  final _retry = RetryEpisode();

  /// 倒计时的每秒刷新器（一轮进行中且已开始重试时才跑）
  Timer? _countdownTimer;

  /// 扫码页是否正在前台：那期间不做自动重连、也不唤起键盘
  /// （否则扫另一台电脑时，首页会立刻去重连旧电脑，甚至把键盘盖到扫码页上）
  bool _scanPageOpen = false;

  /// 是否正在探活（回到前台时先问一句"对面还在吗"）
  bool _probing = false;

  /// 连接尝试的代号：进扫码页会自增，作废还挂在半空中的旧尝试——
  /// 否则它超时后会把刚配对好的界面按回"连接中"（用户实测过）
  int _attemptGen = 0;

  /// 开一轮重试。**只由明确的触发调用**：用户点「连接」/ 回到前台 / 网络变化 /
  /// 冷启动 / 刚断开。已在跑就重置成全新的一轮（环境变了，给足整整一轮）。
  void _beginRetryEpisode() {
    _retry.start(DateTime.now().millisecondsSinceEpoch);
    _reconnectTimer?.cancel();
    _startCountdownTicker(); // 到第一次重试被排上时，胶囊才会显示秒数
  }

  /// 结束一轮：彻底停手，等下一次触发。
  /// 同时作废"在飞的那次尝试"——否则它收尾时会又开一轮（历史事故），
  /// 界面也会多显示一段没有倒计时的「连接中」。
  void _endRetryEpisode() {
    _retry.end();
    _reconnectTimer?.cancel();
    _attemptGen++;
    widget.sender.abortConnectAttempts();
    _connecting = false;
    _countdownTimer?.cancel();
    _countdownTimer = null;
  }

  void _startCountdownTicker() {
    _countdownTimer?.cancel();
    _countdownTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      if (!mounted) return;
      if (!_retry.isActive(DateTime.now().millisecondsSinceEpoch)) {
        // 到点（或已被结束）：停掉 ticker，并让这一轮的界面立刻落回灰点 +「连接」
        _countdownTimer?.cancel();
        _countdownTimer = null;
      }
      setState(() {});
    });
  }

  /// 用户点「中断」：立刻停手、本进程不再自动重试；**记录不动**
  /// （临时连接的窗口照样有效，重启 APP 后也会自动再试）
  void _stopRetrying() {
    widget.sender.manualDisconnected = true;
    _endRetryEpisode();
    setState(() {});
  }

  /// 是否"在忙"：正在连接，或快速重试那一轮还没结束（有排程中的重试）。
  /// "到点"由状态机自己算（`isActive`），不依赖任何一条路径来问一句
  bool get _busy =>
      _connecting ||
      _retry.isActive(DateTime.now().millisecondsSinceEpoch) ||
      _status == ConnectionStatus.connecting ||
      _status == ConnectionStatus.pairing;

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
      _lastNetChangeAt = DateTime.now();
      if (!mounted || _connecting) return;
      if (_status == ConnectionStatus.paired) return;
      if (!_canAutoConnect()) return;
      _beginRetryEpisode(); // 环境变了：重新开始一轮（网络变化本身就是触发）
      unawaited(_attemptConnect());
    });

    // 冷启动：有绑定电脑且不是"本进程已手动断开"状态时自动连接
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_canAutoConnect()) _beginRetryEpisode();
      _attemptConnect();
    });
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    // 可能被调用多次；subscribe 自身幂等
    final route = ModalRoute.of(context);
    if (route != null) appRouteObserver.subscribe(this, route);
  }

  @override
  void dispose() {
    appRouteObserver.unsubscribe(this);
    WidgetsBinding.instance.removeObserver(this);
    _statusSub.cancel();
    _sessionEndedSub.cancel();
    _sessionUpdatedSub.cancel();
    _netSub?.cancel();
    _debounce?.cancel();
    _reconnectTimer?.cancel();
    _transientExpiryTimer?.cancel();
    _countdownTimer?.cancel();
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

    // 判活只在前后台做：后台/Doze 定时器被延迟，那种空档不能算"连接已死"
    widget.sender.tcp.setForeground(state == AppLifecycleState.resumed);

    if (state == AppLifecycleState.resumed) {
      _onResumed();
    }
    // 进入后台：不取消重连排程、不停心跳——切后台不是断开理由，
    // 常用设备/临时行为一致。Doze 下 Timer 被延迟，切回前台在 _onResumed 立即补试。
  }

  AppLifecycleState? _lastLifecycleState;

  Future<void> _onResumed() async {
    // 用户可能在系统设置里改了设备名：顺手同步（毫秒级），下次连接 PC 即显示新名
    unawaited(widget.sender.refreshDeviceName());

    if (_status == ConnectionStatus.paired) {
      // 界面上写着"已连接"不代表真的还连着：判活只在前后台做，后台期间不判定，
      // 所以"在后台时对端走了"这件事本地根本不知道。回到前台先**主动探一下**：
      // 活着就只唤起键盘（省掉一次无谓的重连）；没回应就当作断开，走重连。
      if (_probing) return;
      _probing = true;
      final alive = await widget.sender.tcp.verifyAlive();
      _probing = false;
      if (!mounted) return;
      if (alive) {
        unawaited(_requestKeyboard(from: 'resume'));
        return;
      }
      debugPrint('BT_CONN 回到前台探活失败：连接其实已经断了，开始重连');
      // 让它按"真的断开"处理：状态跳变会触发一轮重连（与平时断线同一条路径）
      unawaited(widget.sender.tcp.disconnect());
      return;
    }

    if (!_canAutoConnect()) return;
    if (_connecting ||
        _status == ConnectionStatus.connecting ||
        _status == ConnectionStatus.pairing) {
      return;
    }
    // 已是断开态（含"一轮重试刚结束"）：回到前台立刻再开一轮、马上试一次
    _beginRetryEpisode();
    _attemptConnect();
  }

  // 有可自动连接的目标：已绑定电脑，或"窗口期内"的临时会话（断线续连）
  bool _canAutoConnect() =>
      widget.sender.hasAnyTarget && !widget.sender.manualDisconnected;

  /// 「已连接」的**唯一收尾入口**：结束重试那一轮、清掉临时到期刷新、丢掉上次
  /// 失败留下的网络判断，并把状态置为已连接。
  ///
  /// **不要绕过它直接给 `_status` 赋值**：收尾（`_endRetryEpisode()`）就挂在这里，
  /// 绕过之后界面会停在"已连接 + 连接中 · N 秒 + 断开"这种自相矛盾的状态
  /// （用户实测报过：连接状态里冒出倒计时）。
  void _markPaired() {
    _status = ConnectionStatus.paired;
    _endRetryEpisode();
    _transientExpiryTimer?.cancel();
    _offLan = false; // 连上了：清掉上次失败留下的网络判断，别残留提示
    unawaited(_requestKeyboard(from: 'paired'));
  }

  void _onStatus(ConnectionStatus status) {
    if (!mounted) return;
    final prev = _status;
    setState(() => _status = status);

    switch (status) {
      case ConnectionStatus.paired:
        _markPaired();
        break;
      case ConnectionStatus.occupied:
        // 电脑正被别人用：**立即停止自动重连**，把"要不要接管"留给用户手动决定。
        _endRetryEpisode();
        if (widget.sender.lastAuthMode != AuthMode.token) {
          // 扫码/临时凭证连的：属于正常的连接竞争，不必解释——清掉临时记录
          // 回扫码引导页（此刻电脑窗口上也没有二维码，用户一看就知道正忙）
          unawaited(widget.sender.abandonTransient());
        }
        break;
      case ConnectionStatus.disconnected:
        _peerClosed = widget.sender.tcp.lastDisconnectWasPeer;
        // 临时接入 + "确定是电脑侧关闭了连接" → 立刻清记录回扫码引导页（不干等窗口期）。
        // 两道护栏缺一不可：① 关闭时我们自己还在正常通信（不是被系统挂起静默到
        // 被 PC 收尸）；② 手机网络没有刚刚变化过（不是我们自己换网络导致的连接结束）
        final receiverSide = peerCloseIsReceiverSide(
          peerClosed: _peerClosed,
          weWereTalking: widget.sender.tcp.peerClosedWhileTalking,
          netJustChanged: _netChangedRecently,
        );
        // 必须确实还握着一条有效的临时记录才处理：清记录这个动作本身会再抛一次
        // "断开"事件，缺这个前提就会自激循环、疯狂弹提示（用户实测踩到过）
        // 判断"这条连接是不是临时身份"要用**会话本身**，不能用
        // "最近一次连接尝试用了什么凭证"——扫码接入的临时连接 lastAuthMode 是 code，
        // 用它判断会让"重启电脑端"这条规则对扫码来的临时连接完全失效（用户实测）。
        if (receiverSide &&
            !_sessionEndedHandled &&
            widget.sender.hasTransientSession) {
          unawaited(_abandonTransientWithNotice());
          break;
        }
        // 主动尝试中的连接失败由 _attemptConnect 收尾统一排程；
        // 这里只处理"**刚断开**"这个事件（状态真的发生跳变时）：
        // 已有的一轮在跑就不动它——否则每次尝试失败都会开新的一轮（历史事故）。
        if (!_scanPageOpen &&
            !_retry.isActive(DateTime.now().millisecondsSinceEpoch) &&
            prev != ConnectionStatus.disconnected &&
            !_connecting &&
            _canAutoConnect()) {
          _beginRetryEpisode();
          _scheduleReconnect();
        }
        break;
      default:
        break;
    }
  }

  /// 服务端主动推送的会话事件（PC 托盘断开、解绑降级、常用设备接管、同身份替换）
  void _onSessionEnded(String reason) {
    _sessionEndedHandled = true;
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
        _showSnack('这台电脑的主人已连接');
        break;
      case 'replaced':
      default:
        // 同身份新连接替换旧连接等内部情形：静默，按普通断线处理
        break;
    }
  }

  /// 临时接入的"接收方出问题"出口：停掉重试、清记录、回扫码引导页，并说明一句
  Future<void> _abandonTransientWithNotice() async {
    if (!widget.sender.hasTransientSession) return; // 已经没什么可作废的了
    _endRetryEpisode();
    _transientExpiryTimer?.cancel();
    await widget.sender.abandonTransient();
    if (!mounted) return;
    _showSnack('连接已失效，请重新扫码');
    setState(() {});
  }

  /// 临时记录快到期时排一次刷新，让界面自己跳回扫码引导页
  void _scheduleTransientExpiryRefresh() {
    _transientExpiryTimer?.cancel();
    final t = widget.sender.storage.transientSession;
    if (t == null) return;
    final ms = t.expiresAtMs - DateTime.now().millisecondsSinceEpoch;
    if (ms <= 0) return;
    _transientExpiryTimer = Timer(Duration(milliseconds: ms + 200), () {
      if (mounted) setState(() {});
    });
  }

  /// 排程一次退避自动重连（前后台一致）。
  ///
  /// [net] 为本机网络快照：**只有移动数据时不重试**——本产品只在同一局域网下能工作，
  /// 连上去也无从谈起，反复重试与广播只是白耗电；Wi-Fi 回来时由网络变化监听重新拉起。
  /// 传 null 表示"还没来得及判断"（突发断线路径），按可能有局域网处理。
  void _scheduleReconnect([LocalNet? net]) {
    // 已经没有可连目标了（记录被清、被拒、被解绑）：这一轮结束，
    // 否则倒计时与「中断」会继续挂在界面上（"未连接电脑 + 连接中 + 倒计时"）
    if (!_canAutoConnect()) {
      _endRetryEpisode();
      return;
    }
    if (_status == ConnectionStatus.paired ||
        _status == ConnectionStatus.occupied ||
        _connecting) {
      return;
    }

    _reconnectTimer?.cancel();

    // 只有移动数据（枚举不到任何局域网网卡）→ 这一轮到此为止，不做无意义的尝试
    if (net != null && !net.hasLan) {
      _endRetryEpisode();
      return;
    }

    // 一轮要不要继续：完全交给状态机（到点 / 超上限都会返回 false）
    final now = DateTime.now().millisecondsSinceEpoch;
    if (!_retry.shouldSchedule(now)) {
      _endRetryEpisode(); // 到点：界面落到灰点 + 「连接」
      return;
    }

    final delayMs = _retry.nextDelayMs(now);
    _reconnectTimer = Timer(Duration(milliseconds: delayMs), _attemptConnect);
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

    final gen = ++_attemptGen; // 本次尝试的代号：中途去扫码就作废
    _connecting = true;
    _sessionEndedHandled = false;
    HelloOutcome? outcome;
    try {
      // 上次已经判明"电脑那个地址不在我任何网段里"→ 跳过无意义的直连，只广播：
      // 电脑可能已经搬到我这个网络（地址也变了），广播能找到它；找不到就停下等触发
      outcome = await widget.sender.autoConnect(discoveryOnly: _offLan);
    } catch (_) {}
    if (!mounted) return;

    // 电脑**明确拒绝**本机凭证：按凭证类型说真话——
    // 绑定手机被拒 = 真被解绑了；临时接入被拒 = 电脑把这次临时连接忘了
    // （PC 服务重启/退出：接收方自身出问题就不给机会）或窗口真过期。
    if (outcome != null && outcome.isUnauthorized) {
      final bound = widget.sender.lastAuthMode == AuthMode.token;
      final id = widget.sender.storage.primaryDevice?.deviceId;
      // 拒绝我们的可能不是"我们那台电脑"（旧地址被别的设备占了）。那种情况
      // 绝不能删绑定——否则一台陌生产品就能让好手机丢掉绑定。
      if (!rejectionFromOurPc(responderId: outcome.deviceId, expectedId: id)) {
        debugPrint('BT_CONN 凭证被拒，但拒绝方不是记忆里那台电脑，保留绑定');
        _connecting = false;
        if (mounted) setState(() {});
        return;
      }
      _endRetryEpisode(); // 绑定没了：重试也没有意义，立刻收尾
      await widget.sender.forgetDevice(id);
      // 同一次失败只弹一句：临时路径也会记一个"已失效"标记，这里顺手消费掉
      widget.sender.consumeTransientExpiredNotice();
      _connecting = false;
      if (mounted) {
        _showSnack(credentialRejectedMessage(bound: bound));
        setState(() {});
      }
      return;
    }

    // 临时会话在本地就已过期（窗口到期）：给出明确出口，而不是静默回到引导页
    final expiredName = widget.sender.consumeTransientExpiredNotice();
    if (expiredName != null) {
      _showSnack('连接已失效，请重新扫码');
    }

    // 已被作废（例如用户中途去扫码了）：什么都不做，绝不碰界面状态或重连排程
    if (gen != _attemptGen) return;

    // 连不上：枚举一次本机网络，同时回答两个问题——
    // ① 是不是根本不在同一个局域网（能判断才提示，判断不了就沉默）；
    // ② 还有没有局域网可试（只有移动数据就不必再折腾了）
    final net = await localNetSnapshot();
    final target = widget.sender.lastAttemptTargetIp;
    if (gen != _attemptGen) return;
    _connecting = false;
    if (!mounted) return;
    // 判断不了（枚举失败）就不提示：同一个 Wi-Fi 下谎报"不在同一网络"更伤
    _offLan = looksOffLan(target, net);
    _scheduleReconnect(net);
    _scheduleTransientExpiryRefresh();
    setState(() {});
  }

  /// 手动点灰钮连接：清除手动断开标记，立即尝试
  Future<void> _manualConnect() async {
    widget.sender.manualDisconnected = false;
    _beginRetryEpisode(); // 用户明确要求连接
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

  /// 把焦点与键盘交还给打字区：**聚焦是必须的，弹键盘是尽力而为**。
  ///
  /// 两个坑（都是真机实测出来的）：
  /// 1. **返回动画期间不能急着聚焦**：那时首页路由还是「被覆盖」状态，Flutter 禁止其
  ///    子树获得焦点，`requestFocus()` 会被**静默拒绝**（canRequestFocus=false）。而
  ///    **手势返回**时 pop 的 future 在动画刚开始就完成（按钮返回要等到动画结束），
  ///    所以不能拿 future 返回当「已回到前台」——要等 `ModalRoute.isCurrent` 为真，
  ///    一拍一拍地等，不假定转场时长。
  /// 2. 「键盘已在显示」只能跳过 `TextInput.show`，绝不能整个函数返回；否则连补一次
  ///    `requestFocus()` 都不做，焦点再也回不来（用户实测：从设置页/扫码页返回后
  ///    "键盘弹了但光标不在"，且时好时坏）。
  ///
  /// 冷启动竞态：自动连接在 Activity 窗口完成 IME 注册前就绪时，`TextInput.show` 会被
  /// 系统取消（ImeTracker: PHASE_CLIENT_ALREADY_HIDDEN，HyperOS 实测复现），所以每次
  /// show 之后都回头验证键盘是否真的弹出（看 viewInsets），没弹出就下一拍再补。
  ///
  /// [from] 只用于出问题时定位是哪个入口进来的。
  Future<void> _requestKeyboard({String from = '?'}) async {
    if (_status != ConnectionStatus.paired) return;
    // 扫码页还在前台：别把键盘盖上去（配对成功会先把那个页面弹掉）
    if (_scanPageOpen) return;
    const step = Duration(milliseconds: 150);
    for (var attempt = 0; attempt < 10; attempt++) {
      if (!mounted || _status != ConnectionStatus.paired) return;
      final route = ModalRoute.of(context);
      if (route != null && !route.isCurrent) {
        await Future<void>.delayed(step);
        continue;
      }
      _focusNode.requestFocus();
      final views = WidgetsBinding.instance.platformDispatcher.views;
      final keyboardShown =
          views.isNotEmpty && views.first.viewInsets.bottom > 0;
      if (_focusNode.hasFocus && keyboardShown) return; // 焦点与键盘都到位
      if (!keyboardShown) {
        await SystemChannels.textInput.invokeMethod<void>('TextInput.show');
      }
      await Future<void>.delayed(step);
    }
    // 诊断（只在失败路径上输出一行，正常时不出声）：焦点始终没拿到，
    // 说明上面两个坑又冒出来了——留这一行免得下次只能干瞪眼
    debugPrint('BT_FOCUS 未拿到焦点（入口=$from status=$_status）');
  }

  /// 子页面（设置页/扫码页/将来任何页面）退场、本页重新回到前台。
  /// **这就是「返回」的正确时机**：按钮返回与手势返回都会走它。
  @override
  void didPopNext() {
    // 这个事件比 pushNamed 的 future **先到**，所以「扫码页开着」的标记要在这里清掉：
    // 否则 _requestKeyboard() 会因为它还是 true 而静默返回 ⇒ 扫码页返回后既不聚焦
    // 也不弹键盘（用户实测；设置页没有这个标记所以不受影响）。
    _scanPageOpen = false;
    unawaited(_requestKeyboard(from: 'popNext'));
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

  /// 上一次弹提示的内容与时刻：同一条消息短时间内不重复弹。
  /// 弹提示的路径一旦被循环触发就会刷屏（历史上真出过），这里兜一层底。
  String? _lastSnackText;
  int _lastSnackMs = 0;

  void _showSnack(String message) {
    final now = DateTime.now().millisecondsSinceEpoch;
    if (_lastSnackText == message && now - _lastSnackMs < 2000) return;
    _lastSnackText = message;
    _lastSnackMs = now;
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
    // 设备名在连接/断开各状态下保持原位，断开不换标题，只换颜色
    final knownName = session?.deviceName ??
        widget.sender.storage.primaryDevice?.name ??
        widget.sender.storage.transientSession?.name;

    // 在忙 = 正在连接或正在重试（有排程中的重试）：点呼吸、词显示"连接中"
    final busy = _busy;
    final connected = _status == ConnectionStatus.paired;
    final hasTarget = widget.sender.hasAnyTarget;
    final secondsLeft =
        _retry.remainingSeconds(DateTime.now().millisecondsSinceEpoch);
    final word = pillWord(busy: busy, secondsLeft: secondsLeft);
    final action = pillAction(
      connected: connected,
      busy: busy,
      hasTarget: hasTarget,
    );
    // 临时连接才在电脑名后面标「临时」：常用是常态、不说话（规则见 transientTag）
    final tag = transientTag(
      busy: busy,
      manuallyDisconnected: widget.sender.manualDisconnected,
      sessionIsPrimary:
          (session != null && session.ok) ? session.isPrimary : null,
      hasBoundDevice: widget.sender.storage.hasDevices,
      hasTransientRecord: widget.sender.hasTransientSession,
    );

    // 未绑定过任何电脑：正文就是扫码引导页，胶囊只说"未连接电脑"
    final unbound = !connected && !hasTarget;

    // 胶囊内文字按钮：收缩触控热区到视觉尺寸，高度不超过定高胶囊
    ButtonStyle pillButtonStyle(Color? color) => TextButton.styleFrom(
          foregroundColor: color,
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          minimumSize: const Size(0, 36),
          padding: const EdgeInsets.symmetric(horizontal: 10),
          visualDensity: VisualDensity.compact,
        );

    final Widget button = switch (action) {
      PillAction.none => const SizedBox.shrink(),
      // "断开"是低频操作：明确置灰，不与主色行动争抢
      PillAction.disconnect => TextButton(
          onPressed: _manualDisconnect,
          style: pillButtonStyle(Colors.grey.shade600),
          child: const Text('断开'),
        ),
      // 连接：连不上、主动断开、电脑被占用（绑定手机点它即接管）都用它
      PillAction.connect => TextButton(
          onPressed: _onPillConnect,
          style: pillButtonStyle(null),
          child: const Text('连接'),
        ),
      // 中断：重试中给的动作——不想等就立刻停手（不是"再连一次"）
      PillAction.stop => TextButton(
          onPressed: _stopRetrying,
          style: pillButtonStyle(Colors.grey.shade600),
          child: const Text('中断'),
        ),
    };

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
              // leading 统一 14 盒：10px 圆点居中其中与按钮对齐
              SizedBox(
                width: 14,
                height: 14,
                child: Center(
                  child: _StatusDot(
                    // 连接中/重试中：用**亮绿色**呼吸——我们预期它就是要连上，
                    // 让点看起来像是在"努力接通"，而不是像已经断开
                    color: busy
                        ? Colors.green.shade500
                        : (connected
                            ? Colors.green.shade600
                            : Colors.grey.shade400),
                    breathing: busy,
                  ),
                ),
              ),
              const SizedBox(width: 12),
              // 设备名 + 词占满中间剩余空间，操作按钮因此永远贴右
              Expanded(
                child: Row(
                  children: [
                    Flexible(
                      child: Text(
                        unbound ? '未连接电脑' : (knownName ?? '电脑'),
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                            fontSize: 15, fontWeight: FontWeight.w600),
                      ),
                    ),
                    if (tag != null) ...[
                      const SizedBox(width: 6),
                      Text(
                        tag,
                        maxLines: 1,
                        style: TextStyle(
                          fontSize: 12,
                          color:
                              scheme.onSurfaceVariant.withValues(alpha: 0.6),
                        ),
                      ),
                    ],
                    if (word != null) ...[
                      const SizedBox(width: 8),
                      // 不参与压缩：名字可以先变短，倒计时必须完整可见
                      Text(
                        word,
                        maxLines: 1,
                        style: TextStyle(
                          fontSize: 12,
                          color: scheme.onSurfaceVariant
                              .withValues(alpha: 0.6),
                        ),
                      ),
                    ],
                  ],
                ),
              ),
              button,
            ],
          ),
        ),
      ),
    );
  }

  /// 点打字区（只在断开态可点）= 按胶囊上的「连接」。
  ///
  /// 边界交给纯函数 editorTapIntent：正在连接/重连中点了**不生效**——那一轮已经在试，
  /// 再触发只会重置倒计时、还可能和轮内正在飞的尝试打架（要停手胶囊上有「中断」）。
  void _onEditorTap() {
    switch (editorTapIntent(
      busy: _busy,
      occupied: _status == ConnectionStatus.occupied,
      bound: widget.sender.lastAuthMode == AuthMode.token,
    )) {
      case EditorTapIntent.none:
        break;
      case EditorTapIntent.takeover:
        unawaited(_takeover());
      case EditorTapIntent.connect:
        unawaited(_manualConnect());
    }
  }

  /// 胶囊右侧「连接」按钮：被占用且本机是这台电脑的绑定手机时 = 接管；其余 = 立即连接
  void _onPillConnect() {
    final takeover = _status == ConnectionStatus.occupied &&
        widget.sender.lastAuthMode == AuthMode.token;
    if (takeover) {
      unawaited(_takeover());
    } else {
      _manualConnect();
    }
  }

  /// 主体输入卡：已连接可输入，其余状态同卡原位禁用，骨架不跳变。
  Widget _buildEditorCard(ColorScheme scheme, {required bool enabled}) {
    // 打字区只讲"该做什么"（状态由胶囊区讲，不复述）：默认一句功能说明，
    // 能判断"不在同一个局域网"时换成那条唯一有用的建议
    final String hintText;
    if (enabled) {
      hintText = '长按键盘语音键即可语音输入文字';
    } else if (_offLan) {
      // 能判断"不在同一个网络"：这是最可操作的一条提示
      hintText = '请确认手机和电脑连着同一个 Wi-Fi';
    } else if (_status == ConnectionStatus.occupied &&
        widget.sender.lastAuthMode == AuthMode.token) {
      // 电脑正被另一台手机使用，而本机是这台电脑的绑定手机：点「连接」即接管
      hintText = '另一台手机正在使用这台电脑；点「连接」即可立即接管';
    } else if (_busy) {
      // 正在连接/重连中：此时点打字区不生效（要停手用胶囊的「中断」），
      // 所以不能承诺「点这里能连」，只说事实
      hintText = '连接成功后即可语音输入文字';
    } else {
      // 断开且可以点：把「这里可以点」和「连上后怎么说」合成一句
      // （不再提胶囊上的连接按钮——它本身就在那儿、一眼可见，多提一次只是多一个名词）
      hintText = '点此区域连接电脑，连上后长按键盘语音键语音输入文字';
    }

    return GestureDetector(
      // 断开时整张卡可点 = 立刻连接；正在连接/重连中点它不生效（边界见 _onEditorTap）
      behavior: HitTestBehavior.opaque,
      onTap: enabled ? null : _onEditorTap,
      child: Card(
      margin: EdgeInsets.zero,
      elevation: 0,
      // 背景/边框不随连接状态切换，避免断开瞬间卡片"变色跳一下"
      color: scheme.surface,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(20),
        side: BorderSide(color: scheme.outlineVariant),
      ),
      child: AbsorbPointer(
        // 未连接时禁用输入框：让点击落到卡片本身（否则会被文本框吃掉）
        absorbing: !enabled,
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
              children: [
                const Spacer(),
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
            Text('在电脑端打开 BeepType，用本机扫描其二维码',
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
    final hasTarget = widget.sender.hasAnyTarget;
    // 未连接且没有可连目标时，正文本身就是扫码引导页（中间有一个大扫码按钮），
    // 顶栏那个扫码图标在同一屏里是重复的 → 该状态不显示它
    final showOnboarding =
        !paired && !hasTarget && _status != ConnectionStatus.occupied;

    return Scaffold(
      appBar: AppBar(
        title: const Text('BeepType'),
        actions: [
          if (!showOnboarding)
            IconButton(
              icon: const Icon(Icons.qr_code_scanner_rounded),
              tooltip: '扫码连接电脑',
              onPressed: _navigateToScan,
            ),
          // 设置：今天页里只有 APP 信息，将来加设置项就加在同一页（不再多一次点击）
          IconButton(
            icon: const Icon(Icons.settings_rounded),
            tooltip: '设置',
            // 返回后由 didPopNext 统一交还焦点与键盘（见 _requestKeyboard）
            onPressed: () => Navigator.of(context).pushNamed('/settings'),
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
                child: paired
                    ? _buildEditorCard(scheme, enabled: true)
                    : showOnboarding
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
    // 进扫码页前先把后台重试彻底停掉并作废在飞的尝试：旧尝试的直连超时事件
    // 会把刚配对好的界面按回"连接中"，甚至继续走它自己的"广播补连"、
    // 把新连接拆掉重连（用户实测：配对成功却一直停在"连接中"，第二次才正常）
    _reconnectTimer?.cancel();
    _transientExpiryTimer?.cancel();
    _attemptGen++;
    widget.sender.abortConnectAttempts();
    _connecting = false;
    _scanPageOpen = true; // 扫码期间不做自动重连、不唤键盘
    setState(() {});
    // 先取好 Navigator，避免 await 之后再碰 context
    final navigator = Navigator.of(context);
    await SystemChannels.textInput.invokeMethod<void>('TextInput.hide');

    final result = await navigator.pushNamed('/scan');
    if (!mounted) return;
    _scanPageOpen = false;
    // "没配对成功就回来"时要把之前那条连接找回来（扫别的电脑失败 / 中途返回），
    // 但**只在确实需要连的时候才开轮**：已连接（或正被占用、正在连接/重试）时
    // 什么都别做——那条连接根本没断，平白开一轮会让界面变成
    // "已连接 + 连接中 · N 秒 + 断开"（用户实测：连着的状态里冒出倒计时）
    final needsReconnect = !_busy &&
        _status != ConnectionStatus.paired &&
        _status != ConnectionStatus.occupied;
    if (result != true && needsReconnect && _canAutoConnect()) {
      _beginRetryEpisode();
      unawaited(_attemptConnect());
    }
    if (result == true) {
      // 配对成功：一切重试状态清零，并以 TCP 层的真实状态为准同步一次界面
      if (widget.sender.tcp.isPairedNow) _markPaired();
    }
    setState(() {});
    // 焦点与键盘的交接由 didPopNext 负责（配对成功那条路 _markPaired 里也会做一遍）
  }
}

/// 状态胶囊左侧的小圆点（绿=已连接，橙=占用，灰=未连接）
class _StatusDot extends StatelessWidget {
  final Color color;

  /// 是否呼吸（正在连接/重试中）：柔和的淡入淡出，而不是硬闪
  final bool breathing;

  const _StatusDot({required this.color, this.breathing = false});

  @override
  Widget build(BuildContext context) {
    final dot = Container(
      width: 10,
      height: 10,
      decoration: BoxDecoration(color: color, shape: BoxShape.circle),
    );
    if (!breathing) return dot;
    // 系统开了"减少动态效果"就不做喘息。此时**不能画成实心绿**——那会被读成
    // "已连接"；退化为半透明绿：静止、但与实心绿（已连接）和灰（断开）都区分得开
    if (MediaQuery.maybeDisableAnimationsOf(context) ?? false) {
      return Opacity(opacity: 0.55, child: dot);
    }
    return _BreathingDot(child: dot);
  }
}

/// 呼吸式提示：透明度 0.35↔1.0、约 1.2 秒一个来回、缓入缓出
class _BreathingDot extends StatefulWidget {
  final Widget child;
  const _BreathingDot({required this.child});

  @override
  State<_BreathingDot> createState() => _BreathingDotState();
}

class _BreathingDotState extends State<_BreathingDot>
    with SingleTickerProviderStateMixin {
  late final AnimationController _controller = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 1200),
  )..repeat(reverse: true);

  late final Animation<double> _opacity = Tween<double>(begin: 1.0, end: 0.35)
      .animate(CurvedAnimation(parent: _controller, curve: Curves.easeInOut));

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return FadeTransition(opacity: _opacity, child: widget.child);
  }
}
