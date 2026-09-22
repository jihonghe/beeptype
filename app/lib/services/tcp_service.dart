import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'package:flutter/foundation.dart';

/// TCP 消息（行分隔 JSON）
class TcpMessage {
  final String type;
  final Map<String, dynamic> data;

  TcpMessage(this.type, this.data);

  String encode() => '${jsonEncode({'type': type, ...data})}\n';

  static TcpMessage? decode(String line) {
    try {
      final json = jsonDecode(line.trim()) as Map<String, dynamic>;
      final type = json['type'] as String;
      json.remove('type');
      return TcpMessage(type, json);
    } catch (_) {
      return null;
    }
  }
}

/// ACK 回复
class AckResult {
  final bool ok;
  final String? code;
  final bool pasted;
  final int srvRecvMs;

  AckResult({
    required this.ok,
    this.code,
    required this.pasted,
    required this.srvRecvMs,
  });
}

/// PC 端把本机"设为常用设备"时下发的长期凭证（明文仅此一次）
class PromotedGrant {
  final String token;
  final String deviceId;
  final String deviceName;
  final String deviceType;
  final Map<String, dynamic> capabilities;
  PromotedGrant({
    required this.token,
    required this.deviceId,
    required this.deviceName,
    required this.deviceType,
    required this.capabilities,
  });
}

/// 降级为临时设备时，PC 补发的续连凭证
/// 电脑端行为开关（手机端只显示与转发；真正的落盘在电脑端）
class PrefsState {
  final bool autoPaste;
  final bool autoSend;
  const PrefsState({required this.autoPaste, required this.autoSend});
}

class ResumeGrant {
  final String token;
  final int expiresInSec;
  ResumeGrant(this.token, this.expiresInSec);
}

/// 保活凭证的当场同步（设置项「临时连接保活」一改，PC 立刻推过来）。
/// token 为空 = 作废，手机应清掉本地记录。
class KeepaliveUpdate {
  final String? token;
  final int expiresInSec;

  const KeepaliveUpdate({this.token, this.expiresInSec = 0});

  /// PC 推过来的时长；<= 0 或缺 token 表示保活被关闭
  bool get disabled => token == null || token!.isEmpty || expiresInSec <= 0;
}

/// hello_ack 的结构化结果
class HelloOutcome {
  final bool ok;

  /// ok | occupied | bad_code | unauthorized | cooldown
  final String? status;

  /// primary | transient（ok 时有效）
  final String? session;
  final String? token;
  final String? deviceId;
  final String? deviceName;
  final String? deviceType;
  final Map<String, dynamic> capabilities;
  final String? error;

  /// 临时会话的续连凭证（仅"新接入的临时会话"下发一次）
  final String? resumeToken;

  /// 续连窗口时长（秒）；临时会话每次握手响应都带，手机据此刷新本地到期时间
  final int? resumeExpiresIn;

  bool get isPrimary => session == 'primary';
  bool get isOccupied => status == 'occupied';

  /// 电脑**明确拒绝**了本机凭证（不认识这台设备）——凭证已失效，应删除并重扫。
  ///
  /// 刻意不含 cooldown：那只是"尝试太频繁，等 60 秒再来"的临时限流，
  /// 把绑定删掉就成了"一次限流换一台好设备重扫"，属于误伤。
  bool get isUnauthorized => status == 'unauthorized';

  /// 被电脑限流（IP 冷却）：等一会儿再来，凭证原样保留
  bool get isRateLimited => status == 'cooldown';

  HelloOutcome({
    required this.ok,
    this.status,
    this.session,
    this.token,
    this.deviceId,
    this.deviceName,
    this.deviceType,
    Map<String, dynamic>? capabilities,
    this.error,
    this.resumeToken,
    this.resumeExpiresIn,
  }) : capabilities = capabilities ?? const {};

  factory HelloOutcome.fromAck(Map<String, dynamic> d) => HelloOutcome(
        ok: d['ok'] as bool? ?? false,
        status: d['status'] as String?,
        session: d['session'] as String?,
        token: d['token'] as String?,
        deviceId: d['device_id'] as String?,
        deviceName: d['device_name'] as String?,
        deviceType: d['device_type'] as String?,
        capabilities:
            (d['capabilities'] as Map?)?.cast<String, dynamic>() ?? const {},
        error: d['error'] as String?,
        resumeToken: d['resume_token'] as String?,
        resumeExpiresIn: (d['resume_expires_in'] as num?)?.toInt(),
      );

  /// 本地构造一个网络层失败结果（超时/连不上）
  factory HelloOutcome.networkFail() =>
      HelloOutcome(ok: false, status: 'network_fail');
}

/// 心跳（pong）回报的 PC 侧权威信息
class HeartbeatInfo {
  /// PC 认定的身份："primary" 常用设备 / "transient" 临时设备；null = 不给声明
  final String? session;

  /// 临时接入的窗口剩余时长（秒）；null = 该连接不是临时席位
  final int? resumeExpiresIn;

  const HeartbeatInfo({this.session, this.resumeExpiresIn});
}

/// 判活是否超时：前台、有基准时刻、且静默超过窗口 → 已死。
///
/// 抽成纯函数是为了能单测：这条判定错一个方向就是"健康连接被误断"或"假死连接一直绿灯"。
bool livenessExpired({
  required DateTime? lastInboundAt,
  required DateTime now,
  required bool foreground,
  required Duration timeout,
}) {
  if (!foreground) return false; // 后台不判定（Doze 会延迟定时器，判了就是误伤）
  if (lastInboundAt == null) return false;
  return now.difference(lastInboundAt) > timeout;
}

/// TCP 连接
class TcpService {
  Socket? _socket;
  StreamSubscription? _subscription;
  bool _isConnected = false;
  bool _isPaired = false;

  final _statusController = StreamController<ConnectionStatus>.broadcast();

  /// 电脑端行为开关（自动粘贴 / 自动发送）的同步流：
  /// 手机刚连上、以及两端任一方向改完，电脑端都会推一次。
  final _prefsController = StreamController<PrefsState>.broadcast();

  /// 服务端主动结束会话的事件流（reason: owner_takeover/owner_disconnect/
  /// replaced/downgraded）。除 downgraded（解绑降级，连接保持）外，
  /// 消息到达后服务端会随即关闭 TCP。
  final _sessionEndedController = StreamController<String>.broadcast();

  /// 会话被降级为临时（PC 端解绑）时，PC 补发的续连凭证。
  /// 降级来的临时会话必须和扫码临时设备一样能断线续连，否则体验不一致。
  final _downgradeGrantController = StreamController<ResumeGrant>.broadcast();
  Stream<ResumeGrant> get downgradeGrantStream =>
      _downgradeGrantController.stream;

  /// PC 端在设置页把本机设为常用设备时的推送（手机据此落盘长期凭证）
  final _promotedController = StreamController<PromotedGrant>.broadcast();
  Stream<PromotedGrant> get promotedStream => _promotedController.stream;

  /// 保活凭证的当场同步（见 [KeepaliveUpdate]）。
  final _keepaliveController = StreamController<KeepaliveUpdate>.broadcast();
  Stream<KeepaliveUpdate> get keepaliveStream => _keepaliveController.stream;

  /// 心跳（pong）里回报的权威信息（见 [HeartbeatInfo]）。
  final _heartbeatController = StreamController<HeartbeatInfo>.broadcast();
  Stream<HeartbeatInfo> get heartbeatStream => _heartbeatController.stream;

  String? _lastIp;
  int? _lastPort;

  /// 最近一次成功连接的端点（落盘临时续连记录时要用）
  String? get lastIp => _lastIp;
  int? get lastPort => _lastPort;

  Completer<HelloOutcome>? _helloCompleter;
  Completer<bool>? _takeoverCompleter;
  Completer<void>? _byeCompleter;
  final _textCompleters = <String, Completer<AckResult>>{};

  Stream<ConnectionStatus> get statusStream => _statusController.stream;

  /// 电脑端行为开关的同步流（自动粘贴 / 自动发送）
  Stream<PrefsState> get prefsStream => _prefsController.stream;
  Stream<String> get sessionEndedStream => _sessionEndedController.stream;

  bool get isConnected => _isConnected;

  /// 已通过认证且持有席位（可以发文字）
  bool get isPaired => _isPaired;

  final StringBuffer _buffer = StringBuffer();

  /// 连接到 PC（TCP 层；认证前状态保持 connecting）
  Future<void> connect(
    String ip,
    int port, {
    Duration timeout = const Duration(seconds: 5),
  }) async {
    if (_isConnected) return;

    final seq = ++_connectSeq;
    // 防御性清理上一轮残留资源
    _stopHeartbeat();
    await _subscription?.cancel();
    _subscription = null;
    _socket?.destroy();
    _socket = null;

    _statusController.add(ConnectionStatus.connecting);

    try {
      final sock = await Socket.connect(ip, port, timeout: timeout);
      if (seq != _connectSeq) {
        // 已被更新的连接取代（例如用户扫码配对）：丢掉这条，别覆盖新连接
        sock.destroy();
        return;
      }
      _socket = sock;
      _lastIp = ip;
      _lastPort = port;

      _isConnected = true;
      _lastInboundAt = DateTime.now();
      _peerClosed = false;
      _localClose = false;

      _subscription = _socket!.listen(
        _onData,
        onError: (error) => _onError('Stream error: $error'),
        onDone: () {
          // 对端关闭（FIN）与本地主动关闭要分开：前者意味着"电脑那边的程序没了"
          _peerClosed = !_localClose;
          _onDisconnected();
        },
      );

      // 吸收 IOSink.done 通道重复上报的写入错误
      unawaited(_socket!.done.catchError((Object _) {}));

      _startHeartbeat();
    } catch (e) {
      // 只有仍是"当代"的尝试才允许抛状态；被取代的旧尝试静默失败
      if (seq == _connectSeq) {
        _statusController.add(ConnectionStatus.disconnected);
      }
      rethrow;
    }
  }

  /// 发送 hello。
  ///
  /// - 绑定/临时接入：传 [code]
  /// - 已绑定常用设备自动连接：传 [token]
  /// - 临时会话断线续连：传 [resume]（窗口期内的续连凭证）
  ///
  /// 返回结构化结果：成功（持席）/ occupied（等待接管）/ 各类拒绝。
  /// 非 occupied 的失败会在内部关闭 TCP。
  Future<HelloOutcome> hello({
    String? code,
    String? token,
    String? resume,
    String? hardwareId,
    /// 本地是否还存着长期凭证（只有扫码路径需要说这件事）。
    /// `false` = 重装/清过数据 → PC 端据此补发新 token，否则我们什么都存不下、
    /// 断线后只能靠临时窗口撑着。`null` = 不提（旧行为，旧电脑也忽略它）。
    bool? hasLocalCredential,
    required String clientId,
    required String deviceName,
    Duration timeout = const Duration(seconds: 10),
  }) async {
    if (!_isConnected) return HelloOutcome.networkFail();

    final completer = Completer<HelloOutcome>();
    _helloCompleter = completer;

    _send(TcpMessage('hello', {
      if (code != null) 'code': code,
      if (token != null) 'token': token,
      if (resume != null) 'resume': resume,
      'client_id': clientId,
      'device_name': deviceName,
      if (hardwareId != null && hardwareId.isNotEmpty) 'hardware_id': hardwareId,
      if (hasLocalCredential != null) 'has_local_credential': hasLocalCredential,
    }));
    if (code != null) {
      _statusController.add(ConnectionStatus.pairing);
    }

    try {
      final outcome = await completer.future.timeout(timeout);
      if (outcome.ok) {
        _isPaired = true;
        _statusController.add(ConnectionStatus.paired);
      } else if (outcome.isOccupied) {
        // 连接保持，等待用户决定是否接管
        _isPaired = false;
        _statusController.add(ConnectionStatus.occupied);
      } else {
        // bad_code / unauthorized / cooldown：关闭连接，交调用方处置
        await _teardown(announce: false);
        _statusController.add(ConnectionStatus.disconnected);
      }
      return outcome;
    } catch (_) {
      await _teardown(announce: false);
      _statusController.add(ConnectionStatus.disconnected);
      return HelloOutcome.networkFail();
    }
  }

  /// 改电脑端的行为开关（自动粘贴 / 自动发送）。
  ///
  /// **成对发送**：这两个开关是一对（自动发送依赖自动粘贴），只发被改动的那个会留下
  /// 依赖不一致的窗口，所以调用方要给出一对目标状态（见 `prefsAfterToggle`）。
  ///
  /// **只有常用设备席位会被电脑端接受**；改完（或被忽略）电脑端都会推一条 `prefs`
  /// 回来，界面据此自动纠正，所以这里不需要返回值。
  void setPrefs({required bool autoPaste, required bool autoSend}) {
    if (!_isConnected) return;
    _send(TcpMessage('set_prefs', {
      'auto_paste': autoPaste,
      'auto_send': autoSend,
    }));
  }

  /// occupied 状态下请求接管电脑（仅认证常用设备可用）
  Future<bool> takeover() async {
    if (!_isConnected) return false;
    final completer = Completer<bool>();
    _takeoverCompleter = completer;
    _send(TcpMessage('takeover', {}));
    try {
      final ok = await completer.future
          .timeout(const Duration(seconds: 8));
      if (ok) {
        _isPaired = true;
        _statusController.add(ConnectionStatus.paired);
      }
      return ok;
    } catch (_) {
      return false;
    }
  }

  /// 主动告别：发 bye 等 bye_ack（兜底 1.5s），再关闭连接
  Future<void> bye() async {
    if (!_isConnected) return;
    _localClose = true;
    final completer = Completer<void>();
    _byeCompleter = completer;
    _send(TcpMessage('bye', {}));
    try {
      await completer.future.timeout(const Duration(milliseconds: 1500));
    } catch (_) {}
    await _teardown(announce: false);
    _isPaired = false;
    _statusController.add(ConnectionStatus.disconnected);
  }

  /// 发送文字（必须已持席）
  Future<AckResult> sendText(String text) async {
    if (!_isConnected || !_isPaired) {
      return AckResult(
        ok: false,
        code: 'not_connected',
        pasted: false,
        srvRecvMs: 0,
      );
    }

    final id = '${DateTime.now().millisecondsSinceEpoch}-${_randomHex(4)}';
    final completer = Completer<AckResult>();
    _textCompleters[id] = completer;

    _send(TcpMessage('text', {
      'id': id,
      'text': text,
      'ts': DateTime.now().millisecondsSinceEpoch,
    }));

    return completer.future.timeout(
      const Duration(seconds: 5),
      onTimeout: () {
        _textCompleters.remove(id);
        return AckResult(
          ok: false,
          code: 'timeout',
          pasted: false,
          srvRecvMs: 0,
        );
      },
    );
  }

  /// 硬断开（无 bye；异常清理用）
  Future<void> disconnect() async {
    // 本来就是断的就不必再抛一次状态：重复抛会让"断开"驱动的界面逻辑自激
    final wasConnected = _isConnected || _socket != null;
    _localClose = true;
    _connectSeq++; // 作废任何在飞的 connect
    await _teardown(announce: false);
    _isPaired = false;
    if (wasConnected) {
      _statusController.add(ConnectionStatus.disconnected);
    }
  }

  /// 释放底层资源；announce=false 表示不广播 disconnected（由调用方决定状态）
  Future<void> _teardown({required bool announce}) async {
    _stopHeartbeat();
    final sub = _subscription;
    _subscription = null;
    await sub?.cancel();
    final sock = _socket;
    _socket = null;
    if (sock != null) {
      sock.destroy();
    }
    _isConnected = false;
    _buffer.clear();

    final hello = _helloCompleter;
    _helloCompleter = null;
    if (hello != null && !hello.isCompleted) {
      hello.complete(HelloOutcome.networkFail());
    }
    final takeover = _takeoverCompleter;
    _takeoverCompleter = null;
    if (takeover != null && !takeover.isCompleted) takeover.complete(false);
    final bye = _byeCompleter;
    _byeCompleter = null;
    if (bye != null && !bye.isCompleted) bye.complete();
    for (final c in _textCompleters.values) {
      if (!c.isCompleted) {
        c.complete(AckResult(
          ok: false,
          code: 'disconnected',
          pasted: false,
          srvRecvMs: 0,
        ));
      }
    }
    _textCompleters.clear();
    if (announce) {
      _statusController.add(ConnectionStatus.disconnected);
    }
  }

  void _send(TcpMessage msg) {
    final socket = _socket;
    if (socket == null) return;
    try {
      socket.write(msg.encode());
    } catch (_) {
      // 对端已消失时 write 可能直接抛：交给 socket 的 onError/onDone 去处理连接状态
    }
  }

  void _onData(List<int> data) {
    _lastInboundAt = DateTime.now(); // 任何服务端消息都算活着的证据
    final probe = _probeCompleter;
    if (probe != null && !probe.isCompleted) probe.complete(true);
    _buffer.write(utf8.decode(data));

    final content = _buffer.toString();
    if (content.contains('\n')) {
      final lines = content.split('\n');
      _buffer.clear();
      _buffer.write(lines.removeLast());
      for (final line in lines) {
        if (line.trim().isEmpty) continue;
        _handleLine(line);
      }
    }
  }

  void _handleLine(String line) {
    final msg = TcpMessage.decode(line);
    if (msg == null) return;
    final d = msg.data;

    switch (msg.type) {
      case 'hello_ack':
        final c = _helloCompleter;
        _helloCompleter = null;
        if (c != null && !c.isCompleted) {
          c.complete(HelloOutcome.fromAck(d));
        }
        break;

      case 'takeover_ack':
        final c = _takeoverCompleter;
        _takeoverCompleter = null;
        if (c != null && !c.isCompleted) {
          c.complete(d['ok'] as bool? ?? false);
        }
        break;

      case 'bye_ack':
        final c = _byeCompleter;
        _byeCompleter = null;
        if (c != null && !c.isCompleted) c.complete();
        break;

      case 'session_ended':
        final reason = d['reason'] as String? ?? 'unknown';
        debugPrint('VL_TCP session_ended: $reason');
        _sessionEndedController.add(reason);
        break;

      case 'promoted':
        // PC 端把这台手机设为常用设备：落盘长期凭证（等同扫码勾选"记住这台手机"）
        final pToken = d['token'] as String?;
        final pDeviceId = d['device_id'] as String?;
        if (pToken != null && pToken.isNotEmpty && pDeviceId != null) {
          _promotedController.add(PromotedGrant(
            token: pToken,
            deviceId: pDeviceId,
            deviceName: d['device_name'] as String? ?? '',
            deviceType: d['device_type'] as String? ?? 'pc',
            capabilities:
                (d['capabilities'] as Map?)?.cast<String, dynamic>() ?? const {},
          ));
          _send(TcpMessage('promoted_ack', {}));
        }
        break;

      case 'resume_grant':
        // 设置项「临时连接保活」改动：PC 当场同步（带 token 签发/续期，不带则作废）
        final kaToken = d['resume_token'] as String?;
        final kaTtl = (d['resume_expires_in'] as num?)?.toInt() ?? 0;
        debugPrint('VL_TCP resume_grant token=${kaToken != null} ttl=$kaTtl');
        _keepaliveController.add(
          KeepaliveUpdate(token: kaToken, expiresInSec: kaTtl),
        );
        break;

      case 'session_downgraded':
        // PC 端解绑本机：连接降级为临时（会话保持，不断开），并可能补发续连凭证
        final grantToken = d['resume_token'] as String?;
        final grantExpires = (d['resume_expires_in'] as num?)?.toInt();
        debugPrint('VL_TCP session_downgraded withGrant=${grantToken != null}');
        if (grantToken != null && grantToken.isNotEmpty) {
          _downgradeGrantController.add(
              ResumeGrant(grantToken, grantExpires ?? 300));
        }
        _sessionEndedController.add('downgraded');
        break;

      case 'ack':
        final id = d['id'] as String?;
        if (id != null) {
          final c = _textCompleters.remove(id);
          if (c != null && !c.isCompleted) {
            c.complete(AckResult(
              ok: d['ok'] as bool? ?? false,
              code: d['code'] as String?,
              pasted: d['pasted'] as bool? ?? false,
              srvRecvMs: d['srv_recv_ms'] as int? ?? 0,
            ));
          }
        }
        break;

      case 'prefs':
        // 电脑端推来的行为开关（自动粘贴 / 自动发送）；没同步过就一直不显示开关
        _prefsController.add(PrefsState(
          autoPaste: d['auto_paste'] as bool? ?? false,
          autoSend: d['auto_send'] as bool? ?? false,
        ));
        break;

      case 'pong':
        // 心跳顺带回报权威信息；两样都没带（不持席/未认证）就什么都不做，绝不瞎猜
        final claim = d['session'] as String?;
        final session =
            (claim == 'primary' || claim == 'transient') ? claim : null;
        final ttl = (d['resume_expires_in'] as num?)?.toInt();
        if (session != null || ttl != null) {
          _heartbeatController.add(
            HeartbeatInfo(session: session, resumeExpiresIn: ttl),
          );
        }
        break;
    }
  }

  void _onError(String message) {
    debugPrint('VL_TCP socket 错误: $message');
    _onDisconnected();
  }

  void _onDisconnected() {
    // 幂等：onError 与 onDone 可能先后触发
    if (!_isConnected && _socket == null) return;

    debugPrint('VL_TCP 连接断开 isPaired=$_isPaired '
        '等待中的请求=${_textCompleters.length}');
    _isPaired = false;
    unawaited(_teardown(announce: true));
  }

  Timer? _heartbeatTimer;

  /// 心跳间隔。
  ///
  /// PC 端连接级读超时为 15s（3 个间隔的容错），切后台 Timer 被 Doze
  /// 延迟时也有足够余量；恢复后首个 ping 立即探活。
  /// 心跳间隔：2 秒。判活窗口是它的 3 倍，容忍连续 3 次丢失。
  static const Duration heartbeatInterval = Duration(seconds: 2);

  /// 判活超时：心跳发出后这么久没收到**任何**服务端消息，就判定连接已死。
  ///
  /// 为什么必须有：对端只是断网（关 Wi-Fi、拔网线、路由器重启）时，TCP 不会报错
  /// ——没有 FIN 也没有 RST，手机上这条连接看起来一切正常。旧实现的心跳是"只发不问"，
  /// 于是界面一直显示"已连接"（绿灯），既不重连也感知不到，直到用户重开 APP。
  ///
  /// 6 秒 = 3 次心跳：既够灵敏（最慢 8 秒内从"已连接到"断开"，观感上不迟钝），
  /// 又留了 3 次丢包余量（TCP 本身会重传，真要 6 秒一声不响多半是链路真断了）。
  static const Duration livenessTimeout = Duration(seconds: 6);

  /// 最近一次收到服务端消息的时刻（判活基准）
  DateTime? _lastInboundAt;

  /// 是否在前台：只在前后台判定，且切回前台重置基准——后台/Doze 期间定时器被系统
  /// 延迟，拿休眠前的时刻判定会把健康连接误判成已死
  bool _foreground = true;

  /// 这次断开是不是**对端关闭了连接**（收到 FIN/RST，典型：电脑上退出程序、重启应用）。
  ///
  /// 与"静默超时"区分开：它是个**确定信号**，用于判定"是谁的问题"——
  /// 临时接入碰上它就直接回扫码引导页（接收方自身出问题不给机会），
  /// 而静默超时还要先看手机自己的网络有没有变。
  bool _peerClosed = false;

  /// 本次是不是我们自己主动关的（主动断开/说再见），用来把"对端关闭"区分出来
  bool _localClose = false;

  /// 最近一次断开是否由对端关闭造成
  bool get lastDisconnectWasPeer => _peerClosed;

  /// 主动探活：发一个 ping，等**任何**服务端消息回来。
  ///
  /// 用在"回到前台"：那时本地状态可能还挂在"已连接"，而连接其实早就断了——
  /// 判活只在前后台做，后台期间不判定，所以后台时断掉的连接不会被发现。
  /// 不探一下，界面就会一直显示已连接、也不去重连（用户实测：切回来不重连，
  /// 而且时好时坏——那"好"的几次其实是被系统杀掉后冷启动走了另一条路）。
  Future<bool> verifyAlive({
    Duration timeout = const Duration(milliseconds: 1200),
  }) async {
    if (!_isConnected || _socket == null) return false;
    final completer = Completer<bool>();
    _probeCompleter = completer;
    try {
      _send(TcpMessage('ping', {}));
      return await completer.future.timeout(timeout);
    } catch (_) {
      return false;
    } finally {
      _probeCompleter = null;
    }
  }

  /// 当前是否真的处于已配对状态（界面在扫码返回后用它同步一次，
  /// 免得被任何残留事件把绿色按回"连接中"）
  bool get isPairedNow => _isPaired;

  /// 这次"对端关闭连接"发生时，我们自己还在正常通信吗？
  ///
  /// 锁屏/切后台时系统会挂起本进程的定时器——我们不再发心跳，PC 端 15 秒收不到心跳
  /// 就会主动关掉这条连接。那种 FIN 的责任在**手机侧**（是我们自己没说话），
  /// 不能当成"电脑退出了"，否则临时接入会把还能续连的记录白白丢掉。
  bool get peerClosedWhileTalking {
    if (!_peerClosed) return false;
    final last = _lastInboundAt;
    if (last == null) return false;
    // 15 秒 = PC 端的心跳超时（HEARTBEAT_TIMEOUT_SECS）
    return DateTime.now().difference(last) <= _silenceLimit;
  }

  static const Duration _silenceLimit = Duration(seconds: 15);

  /// 探活等待器：收到**任何**服务端消息就完成（见 verifyAlive）
  Completer<bool>? _probeCompleter;

  /// 连接序号：每发起一次 connect 自增，`disconnect()` 也自增。
  ///
  /// 用来作废**被取代的**那次连接：旧尝试的超时不能抛"断开"状态事件（会把刚配对好的
  /// 界面按回"连接中"），也不能把自己迟到建好的 socket 覆盖上去（会把新连接顶掉）。
  int _connectSeq = 0;

  /// 前后台切换：切回前台时重置判活基准，避免拿休眠期间的空档误判
  void setForeground(bool value) {
    _foreground = value;
    if (value) _lastInboundAt = DateTime.now();
  }

  void _startHeartbeat() {
    _heartbeatTimer?.cancel();
    _heartbeatTimer = Timer.periodic(heartbeatInterval, (_) {
      if (!_isConnected) return;
      // 先判活：连"任何"服务端消息都收不到这么久，说明连接已经假死
      if (livenessExpired(
        lastInboundAt: _lastInboundAt,
        now: DateTime.now(),
        foreground: _foreground,
        timeout: livenessTimeout,
      )) {
        debugPrint('VL_TCP 判活超时（${livenessTimeout.inSeconds}s 无任何回应），判定连接已断');
        _onError('liveness timeout');
        return;
      }
      _send(TcpMessage('ping', {}));
    });
  }

  void _stopHeartbeat() {
    _heartbeatTimer?.cancel();
    _heartbeatTimer = null;
  }

  String _randomHex(int length) {
    final random = DateTime.now().microsecondsSinceEpoch;
    return random.toRadixString(16).padLeft(length, '0').substring(0, length);
  }

  void dispose() {
    _stopHeartbeat();
    unawaited(_subscription?.cancel());
    _socket?.destroy();
    _statusController.close();
    _prefsController.close();
    _sessionEndedController.close();
    _downgradeGrantController.close();
    _keepaliveController.close();
    _heartbeatController.close();
    _promotedController.close();
  }
}

/// 连接状态
enum ConnectionStatus {
  disconnected,
  connecting,
  connected,
  pairing,
  paired,
  occupied,
  failed,
}
