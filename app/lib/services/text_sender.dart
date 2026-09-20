import 'dart:async';
import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';
import 'tcp_service.dart';
import 'storage_service.dart';
import 'discovery_service.dart';

/// 文字发送服务：协调存储、UDP 发现、TCP 连接与配对。
class TextSender {
  final TcpService _tcp = TcpService();
  final StorageService _storage = StorageService();
  final DiscoveryService _discovery = DiscoveryService();

  bool _initialized = false;

  /// 当前会话信息（paired / occupied 时有效），用于 UI 显示电脑名与身份角标
  HelloOutcome? session;

  /// 用户在本进程内主动断开（绿灯）或被 PC 端请出（owner_disconnect）。
  /// 为 true 时绝不自动重连；手动点灰钮或冷启动才清除。
  bool manualDisconnected = false;

  TcpService get tcp => _tcp;
  StorageService get storage => _storage;

  /// 临时会话刚刚失效（本地超时，或 PC 侧拒绝续连）时记下它的名字，
  /// 供 UI 提示一次"请重新扫码"；取走后清空。
  String? _transientExpiredName;
  String? consumeTransientExpiredNotice() {
    final name = _transientExpiredName;
    _transientExpiredName = null;
    return name;
  }

  /// 是否有可自动连接的目标：已绑定主力电脑，或未过期的临时会话记录
  bool get hasAnyTarget => _storage.hasDevices || hasTransientSession;

  /// 是否有未过期的临时会话记录（临时连接的续连窗口内）
  bool get hasTransientSession {
    final t = _storage.transientSession;
    return t != null && !t.isExpired;
  }

  /// 硬件指纹。Android: ANDROID_ID；iOS: Keychain 持久化 UUID。
  /// 两端都在同一个 MethodChannel('beetype/device') 上响应 getHardwareId。
  /// 跨 APP 重装稳定，用于 PC 端识别"重装后仍是同一台手机"。
  String? _hardwareId;
  Future<String?> get hardwareId async {
    if (_hardwareId != null) return _hardwareId;
    try {
      _hardwareId = await const MethodChannel('beetype/device')
          .invokeMethod<String>('getHardwareId');
    } catch (_) {
      _hardwareId = null;
    }
    return _hardwareId;
  }

  /// 自动连接 TCP 直连超时（缓存 IP 失活场景，包被丢弃时挂到超时）
  static const Duration _directConnectTimeout = Duration(milliseconds: 2500);
  static const Duration _helloTimeout = Duration(seconds: 5);

  Future<void> init() async {
    if (_initialized) return;
    await _storage.init();
    // 被降级为临时连接时，PC 会补发续连凭证：立刻落盘，
    // 这样"降级来的临时会话"和扫码临时连接一样能断线续连
    _downgradeSub = _tcp.downgradeGrantStream.listen(_onDowngradeGrant);
    _initialized = true;
  }

  StreamSubscription<ResumeGrant>? _downgradeSub;

  void _onDowngradeGrant(ResumeGrant grant) {
    final ip = _tcp.lastIp;
    final port = _tcp.lastPort;
    final s = session;
    if (ip == null || port == null || s == null) return;
    unawaited(_storage.saveTransientSession(TransientSession(
      deviceId: s.deviceId ?? '',
      name: s.deviceName ?? ip,
      ip: ip,
      port: port,
      token: grant.token,
      expiresAtMs:
          DateTime.now().millisecondsSinceEpoch + grant.expiresInSec * 1000,
    )));
    debugPrint('VL_TCP 降级续连凭证已落盘（${grant.expiresInSec}s 窗口）');
  }

  /// 重新读取系统设备名（切回前台时调用；用户改手机名后，下次连接 PC 即显示新名）
  Future<void> refreshDeviceName() => _storage.syncDeviceName();

  bool get hasDevices => _storage.hasDevices;

  String get clientId => _storage.clientId;
  String get deviceName => _storage.getDeviceName();

  /// 自动连接最近使用的主力电脑。
  ///
  /// 缓存 IP 直连与 UDP 发现并行：直连立即发起；直连网络层失败时，
  /// 用发现结果（proof 门控，应答必为该电脑）对新 IP 补连一次，
  /// 典型 DHCP 换 IP 场景约 3s 内完成。
  Future<HelloOutcome> autoConnect() async {
    manualDisconnected = false;
    final dev = _storage.primaryDevice;
    if (dev == null) {
      // 没有主力凭证：看看有没有"窗口期内"的临时会话可以续连
      return _resumeTransientIfAny();
    }

    // 发现并行开跑（约 1.1s 出结果），不阻塞直连
    final discFuture = _discovery
        .discover(
          clientId: _storage.clientId,
          token: dev.token,
          port: dev.port,
        )
        .catchError((Object _) => <DiscoveredDevice>[]);

    HelloOutcome? outcome;
    final hid = await hardwareId;
    try {
      await _tcp.connect(dev.ip, dev.port, timeout: _directConnectTimeout);
      outcome = await _tcp.hello(
        token: dev.token,
        clientId: _storage.clientId,
        deviceName: _storage.getDeviceName(),
        hardwareId: hid,
        timeout: _helloTimeout,
      );
    } catch (_) {
      outcome = null; // TCP 建连失败
    }

    // 成功 / 占用 / 凭证被拒：结论已明确，无需换 IP 再试
    if (outcome != null &&
        (outcome.ok || outcome.isOccupied || outcome.isUnauthorized)) {
      if (outcome.ok) {
        unawaited(_persistPrimary(dev, outcome, connectedIp: dev.ip));
      } else if (outcome.isOccupied) {
        session = outcome;
      }
      return outcome;
    }

    // 网络层失败：取发现结果补连（DHCP 换 IP 的主路径）
    List<DiscoveredDevice> found = const [];
    try {
      found = await discFuture.timeout(const Duration(seconds: 2));
    } catch (_) {
      found = const [];
    }
    final cand = found.isEmpty ? null : found.first;
    if (cand == null || cand.ip == dev.ip) {
      return outcome ?? HelloOutcome.networkFail();
    }

    await _tcp.disconnect();
    try {
      await _tcp.connect(cand.ip, cand.port, timeout: _directConnectTimeout);
      outcome = await _tcp.hello(
        token: dev.token,
        clientId: _storage.clientId,
        deviceName: _storage.getDeviceName(),
        hardwareId: hid,
        timeout: _helloTimeout,
      );
    } catch (_) {
      return HelloOutcome.networkFail();
    }
    if (outcome.ok) {
      unawaited(_persistPrimary(dev, outcome, connectedIp: cand.ip));
    } else if (outcome.isOccupied) {
      session = outcome;
    }
    return outcome;
  }

  /// 临时会话续连：临时手机被系统杀进程后，凭窗口期内的凭证免扫码回到同一台电脑。
  ///
  /// 失败分三种，处理不同：
  /// - 网络层连不上 → 返回失败，留着记录等下次重试（可能是电脑睡了/换网了）
  /// - 凭证被拒（窗口过期 / PC 重启 / 主人在电脑上断开了它）→ 删记录 + 提示重新扫码
  /// - 席位被占 → 交给上层显示"电脑使用中"（临时方没有接管入口）
  Future<HelloOutcome> _resumeTransientIfAny() async {
    final t = _storage.transientSession;
    if (t == null) return HelloOutcome.networkFail();

    if (t.isExpired) {
      await _storage.clearTransientSession();
      _transientExpiredName = t.name;
      return HelloOutcome.networkFail();
    }

    final hid = await hardwareId;
    try {
      await _tcp.connect(t.ip, t.port, timeout: _directConnectTimeout);
    } catch (_) {
      return HelloOutcome.networkFail();
    }

    final outcome = await _tcp.hello(
      resume: t.token,
      clientId: _storage.clientId,
      deviceName: _storage.getDeviceName(),
      hardwareId: hid,
      timeout: _helloTimeout,
    );

    if (outcome.ok) {
      session = outcome;
      // PC 侧给的窗口时长是权威值（设置可能被改过）：据此刷新本地到期时间
      await _storage.touchTransientSession(
        expiresInSec: outcome.resumeExpiresIn,
      );
    } else if (outcome.isUnauthorized) {
      await _storage.clearTransientSession();
      _transientExpiredName = t.name;
    }
    return outcome;
  }

  /// 扫码配对入口。
  ///
  /// - PC 勾选了"记住此手机"→ ack 带 token，落盘为主力电脑；
  /// - 否则是临时会话，只存内存，不落任何凭证。
  Future<HelloOutcome> pairWithCode(
    String ip,
    int port,
    String code,
  ) async {
    await _tcp.disconnect();
    try {
      await _tcp.connect(ip, port);
    } catch (_) {
      return HelloOutcome.networkFail();
    }

    final hid = await hardwareId;
    final outcome = await _tcp.hello(
      code: code,
      clientId: _storage.clientId,
      deviceName: _storage.getDeviceName(),
      hardwareId: hid,
    );

    if (outcome.ok) {
      session = outcome;
      manualDisconnected = false;
      if (outcome.token != null &&
          outcome.token!.isNotEmpty &&
          outcome.deviceId != null) {
        await _storage.upsertDevice(OutputDevice(
          deviceId: outcome.deviceId!,
          name: outcome.deviceName ?? ip,
          deviceType: outcome.deviceType ?? 'pc',
          ip: ip,
          port: port,
          token: outcome.token!,
          capabilities: outcome.capabilities,
          lastConnectedMs: DateTime.now().millisecondsSinceEpoch,
        ));
      } else if (outcome.resumeToken != null &&
          outcome.resumeToken!.isNotEmpty) {
        // 临时会话：不落长期凭证，只存一条短期的续连记录（窗口期内免扫码回来）
        await _storage.saveTransientSession(TransientSession(
          deviceId: outcome.deviceId ?? '',
          name: outcome.deviceName ?? ip,
          ip: ip,
          port: port,
          token: outcome.resumeToken!,
          expiresAtMs: DateTime.now().millisecondsSinceEpoch +
              (outcome.resumeExpiresIn ?? 300) * 1000,
        ));
      }
    }
    return outcome;
  }

  /// 扫码配对（QR 格式 beetype://<ip>:<port>?code=<code>）
  Future<({bool ok, String? error})> pairFromQr(String qrData) async {
    final Uri uri;
    try {
      uri = Uri.parse(qrData);
    } catch (_) {
      return (ok: false, error: '二维码格式不正确');
    }
    if (uri.scheme != 'beetype' || uri.host.isEmpty) {
      return (ok: false, error: '不是 BeeType 的二维码');
    }
    final code = uri.queryParameters['code'];
    if (code == null || code.isEmpty) {
      return (ok: false, error: '二维码缺少配对码');
    }
    final port = uri.hasPort ? uri.port : 8765;
    final outcome = await pairWithCode(uri.host, port, code);
    return (ok: outcome.ok, error: outcome.ok ? null : _describe(outcome));
  }

  /// occupied 状态下用户点"接管电脑"
  Future<bool> takeover() async {
    final ok = await _tcp.takeover();
    if (ok && session != null) {
      // takeover 后身份仍为 primary（能走到这的必然是认证主力）
      session = HelloOutcome(
        ok: true,
        status: 'ok',
        session: session!.session,
        deviceId: session!.deviceId,
        deviceName: session!.deviceName,
        deviceType: session!.deviceType,
        capabilities: session!.capabilities,
      );
    }
    return ok;
  }

  /// 用户主动断开（绿灯）：bye 优雅告别 + 本进程不再自动重连
  Future<void> userDisconnect() async {
    // 先置标记：bye 会同步广播 disconnected，避免那一帧排程出自动重连
    manualDisconnected = true;
    await _tcp.bye();
  }

  /// PC 端托盘主动断开（owner_disconnect）时同样视为"用户意图的断开"
  void markDisconnectedByPc() {
    manualDisconnected = true;
  }

  /// PC 端解绑本机：会话降级为临时连接（连接保持），删除本地落盘凭证。
  ///
  /// 与 forgetDevice 的区别：不发 bye、不清理内存会话——TCP 会话继续用于
  /// 打字粘贴，只是身份从主力变为临时；断线后不再自动重连（无凭证）。
  Future<void> downgradeToTemp() async {
    final id = session?.deviceId;
    if (id != null) {
      await _storage.removeDevice(id);
    }
    final s = session;
    if (s != null && s.ok) {
      session = HelloOutcome(
        ok: s.ok,
        status: s.status,
        session: 'transient',
        deviceId: s.deviceId,
        deviceName: s.deviceName,
        deviceType: s.deviceType,
        capabilities: s.capabilities,
      );
    }
  }

  /// 忘记一台电脑（删除本地凭证）；若正连着它先告别
  Future<void> forgetDevice(String? deviceId) async {
    final id = deviceId ?? session?.deviceId;
    if (id == null) return;
    if (_tcp.isConnected && session?.deviceId == id) {
      await _tcp.bye();
    }
    await _storage.removeDevice(id);
    if (session?.deviceId == id) session = null;
  }

  Future<AckResult> sendText(String text) => _tcp.sendText(text);

  Future<void> _persistPrimary(
    OutputDevice dev,
    HelloOutcome outcome, {
    required String connectedIp,
  }) async {
    session = outcome;
    await _storage.touchDevice(
      dev.deviceId,
      ip: connectedIp,
      port: dev.port,
      name: outcome.deviceName,
      capabilities: outcome.capabilities,
    );
  }

  /// 把协议状态翻成用户能懂的一句话
  static String _describe(HelloOutcome o) {
    switch (o.status) {
      case 'bad_code':
        return '二维码已失效，请扫描电脑窗口上的最新二维码';
      case 'unauthorized':
      case 'cooldown':
        return '电脑拒绝了本次连接';
      case 'occupied':
        return '电脑正被另一台手机使用';
      case 'network_fail':
      default:
        return '连不上电脑，请检查 IP 和网络';
    }
  }

  void dispose() {
    _downgradeSub?.cancel();
    _tcp.dispose();
  }
}
