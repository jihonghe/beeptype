import 'dart:async';
import 'package:flutter/foundation.dart';

import 'lan_check.dart';
import 'package:flutter/services.dart';
import 'tcp_service.dart';
import 'storage_service.dart';
import 'discovery_service.dart';

/// 一次连接尝试用的凭证类型
enum AuthMode {
  /// 还没尝试过
  none,

  /// 长期凭证（本机是这台电脑的绑定手机）
  token,

  /// 窗口期内的临时续连凭证
  resume,

  /// 扫码配对
  code,
}

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

  /// 是否有可自动连接的目标：已绑定常用电脑，或未过期的临时会话记录
  bool get hasAnyTarget => _storage.hasDevices || hasTransientSession;

  /// 是否有未过期的临时会话记录（临时设备的续连窗口内）
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
    // 被降级为临时设备时，PC 会补发续连凭证：立刻落盘，
    // 这样"降级来的临时会话"和扫码临时设备一样能断线续连
    _downgradeSub = _tcp.downgradeGrantStream.listen(_onDowngradeGrant);
    _promotedSub = _tcp.promotedStream.listen(_onPromoted);
    // 心跳里的权威信息：身份自愈 + 同步临时窗口（PC 是权威方）
    _identitySub = _tcp.heartbeatStream.listen(_onHeartbeat);
    _initialized = true;
  }

  StreamSubscription<ResumeGrant>? _downgradeSub;
  StreamSubscription<PromotedGrant>? _promotedSub;
  StreamSubscription<HeartbeatInfo>? _identitySub;

  /// 上一次把窗口时长写回本地的时刻（节流：心跳 2 秒一次，不必每次都写盘）
  int? _lastTtlSyncMs;

  /// 会话身份发生变化（例如被电脑设为常用设备）时通知界面刷新
  final _sessionUpdatedController = StreamController<void>.broadcast();
  Stream<void> get sessionUpdated => _sessionUpdatedController.stream;

  /// PC 端在设置页把本机设为常用设备：落盘长期凭证 + 作废临时续连记录 + 刷新界面
  Future<void> _onPromoted(PromotedGrant g) async {
    final ip = _tcp.lastIp;
    final port = _tcp.lastPort;
    if (ip == null || port == null || g.token.isEmpty || g.deviceId.isEmpty) {
      debugPrint('VL_TCP promoted 缺少端点信息，忽略');
      return;
    }
    await _storage.upsertDevice(OutputDevice(
      deviceId: g.deviceId,
      name: g.deviceName.isEmpty ? ip : g.deviceName,
      deviceType: g.deviceType,
      ip: ip,
      port: port,
      token: g.token,
      capabilities: g.capabilities,
      lastConnectedMs: DateTime.now().millisecondsSinceEpoch,
    ));
    // 已是常用设备：临时续连记录不再需要
    await _storage.clearTransientSession();
    final s = session;
    if (s != null) {
      session = HelloOutcome(
        ok: s.ok,
        status: s.status,
        session: 'primary',
        deviceId: s.deviceId,
        deviceName: s.deviceName,
        deviceType: s.deviceType,
        capabilities: s.capabilities,
      );
    }
    _sessionUpdatedController.add(null);
    debugPrint('VL_TCP 已被设为常用设备，凭证已落盘');
  }

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

  /// 心跳里带回的 PC 侧权威信息：纠正身份 + 同步临时窗口。
  ///
  /// 窗口同步必须做：手机本地记的到期时间只在"配对/续连成功"时刷新，而 PC 那边的
  /// 窗口是每次心跳都在往后滑的——不同步的话，连接正常用了超过窗口时长之后，
  /// 手机就会自以为过期，一断网就把还能用的记录丢掉、把用户赶去重新扫码。
  void _onHeartbeat(HeartbeatInfo info) {
    final s = session;
    if (s == null || !s.ok) return;

    // 身份自愈：电脑说我是临时接入，而我以为是常用设备 → 按降级处理（删本地凭证）
    if (info.session == 'transient' && s.isPrimary) {
      debugPrint('VL_TCP 心跳纠正身份：常用设备 → 临时接入');
      unawaited(downgradeToTemp().then((_) => _sessionUpdatedController.add(null)));
    } else if (info.session == 'primary' && !s.isPrimary) {
      debugPrint('VL_TCP 心跳纠正身份：临时接入 → 常用设备');
      session = HelloOutcome(
        ok: s.ok,
        status: s.status,
        session: 'primary',
        deviceId: s.deviceId,
        deviceName: s.deviceName,
        deviceType: s.deviceType,
        capabilities: s.capabilities,
      );
      _sessionUpdatedController.add(null);
    }

    // 窗口同步（节流 30 秒写一次）
    final ttl = info.resumeExpiresIn;
    if (ttl != null && !(session?.isPrimary ?? true)) {
      final now = DateTime.now().millisecondsSinceEpoch;
      if (_lastTtlSyncMs == null || now - _lastTtlSyncMs! > 30000) {
        _lastTtlSyncMs = now;
        unawaited(_storage.touchTransientSession(expiresInSec: ttl));
      }
    }
  }

  /// 重新读取系统设备名（切回前台时调用；用户改手机名后，下次连接 PC 即显示新名）
  Future<void> refreshDeviceName() => _storage.syncDeviceName();

  bool get hasDevices => _storage.hasDevices;

  String get clientId => _storage.clientId;
  String get deviceName => _storage.getDeviceName();

  /// 上一次**实际尝试**过的目标地址（连接失败时也要有值——TCP 层的 lastIp
  /// 只在连上后才记，判断"是不是不在同一网络"必须用尝试过的那个地址）
  String? _lastAttemptTargetIp;

  /// 本次尝试用的凭证类型。
  ///
  /// 手机端只有一个判断依赖它：**被占用**时——用绑定凭证连的（本机就是这台电脑的
  /// 绑定手机）才提示"点「连接」即可立即接管"；扫码/临时凭证连的属于正常竞争，
  /// 不给提示、直接回扫码引导页。
  AuthMode lastAuthMode = AuthMode.none;

  /// 放弃当前临时会话：清掉续连记录并断开。
  /// 用在"临时接入撞上电脑被占用"——席位是别人的，留着重连记录没有意义，
  /// 界面回到扫码引导页（要不要再连，交给用户手动决定）。
  Future<void> abandonTransient() async {
    await _storage.clearTransientSession();
    await _tcp.disconnect();
    session = null;
    _sessionUpdatedController.add(null);
  }

  /// 上一次实际尝试过的目标地址（界面判断"是否不在同一网络"用）
  String? get lastAttemptTargetIp => _lastAttemptTargetIp;

  /// 目标地址是否看起来不在同一局域网（供失败提示；判断不了就说"否"）
  Future<bool> targetLooksOffLan() async {
    final ip = _lastAttemptTargetIp;
    if (ip == null) return false;
    final net = await localNetSnapshot();
    return likelyOffLan(ip, net?.ips ?? const []);
  }

  /// 自动连接最近使用的常用电脑。
  ///
  /// 缓存 IP 直连与 UDP 发现并行：直连立即发起；直连网络层失败时，
  /// 用发现结果（proof 门控，应答必为该电脑）对新 IP 补连一次，
  /// 典型 DHCP 换 IP 场景约 3s 内完成。
  Future<HelloOutcome> autoConnect() async {
    manualDisconnected = false;
    final dev = _storage.primaryDevice;
    if (dev == null) {
      // 没有长期凭证：看看有没有"窗口期内"的临时会话可以续连
      return _resumeTransientIfAny();
    }
    lastAuthMode = AuthMode.token;

    HelloOutcome? outcome;
    final hid = await hardwareId;
    _lastAttemptTargetIp = dev.ip;
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

    // 网络层失败：广播找一次（DHCP 换 IP 的主路径）。
    // 只在直连失败后才广播——正常重连（电脑还在、只是刚断）全程零广播，
    // 省掉每次尝试那 1.4 秒的白等与无线电唤醒。
    List<DiscoveredDevice> found = const [];
    try {
      found = await _discovery
          .discover(
            clientId: _storage.clientId,
            token: dev.token,
            port: dev.port,
          )
          .timeout(const Duration(seconds: 2))
          .catchError((Object _) => <DiscoveredDevice>[]);
    } catch (_) {
      found = const [];
    }
    final cand = found.isEmpty ? null : found.first;
    if (cand == null || cand.ip == dev.ip) {
      // 常用记录网络层不可达、广播也没人应：还留着一条**同一台电脑**的有效临时
      // 续连记录时，改走它（解绑后用临时身份重连过就会这样：旧记录挡住能用的新记录）
      final fallback = await _resumeTransientIfSameDevice(dev.deviceId);
      // 临时续连给出明确结论就用它（成功/占用/被拒/被限流）；
      // 只是网络层没连上，就仍旧按常用记录的结果回报
      if (fallback.ok ||
          fallback.isOccupied ||
          fallback.isUnauthorized ||
          fallback.isRateLimited) {
        return fallback;
      }
      return outcome ?? HelloOutcome.networkFail();
    }

    await _tcp.disconnect();
    _lastAttemptTargetIp = cand.ip;
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

  /// 临时设备续连：临时手机被系统杀进程后，凭窗口期内的凭证免扫码回到同一台电脑。
  ///
  /// 失败分三种，处理不同：
  /// - 网络层连不上 → 返回失败，留着记录等下次重试（可能是电脑睡了/换网了）
  /// - 凭证被拒（窗口过期 / PC 重启 / 主人在电脑上断开了它）→ 删记录 + 提示重新扫码
  /// - 席位被占 → 交给上层显示"电脑使用中"（临时方没有接管入口）
  Future<HelloOutcome> _resumeTransientIfAny() => _resumeTransient(null);

  /// 仅当临时记录属于指定电脑时续连（用于"旧常用记录挡路"的回退路径）
  Future<HelloOutcome> _resumeTransientIfSameDevice(String deviceId) =>
      _resumeTransient(deviceId);

  Future<HelloOutcome> _resumeTransient(String? onlyDeviceId) async {
    final t = _storage.transientSession;
    if (t == null) return HelloOutcome.networkFail();
    lastAuthMode = AuthMode.resume;
    if (onlyDeviceId != null && t.deviceId != onlyDeviceId) {
      return HelloOutcome.networkFail();
    }

    if (t.isExpired) {
      await _storage.clearTransientSession();
      _transientExpiredName = t.name;
      return HelloOutcome.networkFail();
    }

    final hid = await hardwareId;
    _lastAttemptTargetIp = t.ip;
    try {
      await _tcp.connect(t.ip, t.port, timeout: _directConnectTimeout);
    } catch (_) {
      // 直连不上（典型：电脑换了 IP）：广播找一次。发现服务认得窗口期内有效的
      // 临时凭证，因此临时设备也能像常用设备那样找回换过地址的电脑。
      // 换了网络（广播出不去）或窗口过期则找不到，保留记录等下次重试。
      final found = await _discovery
          .discover(
            clientId: _storage.clientId,
            token: t.token,
            port: t.port,
          )
          .catchError((Object _) => <DiscoveredDevice>[]);
      if (found.isEmpty) return HelloOutcome.networkFail();
      _lastAttemptTargetIp = found.first.ip;
      try {
        await _tcp.connect(found.first.ip, found.first.port,
            timeout: _directConnectTimeout);
      } catch (_) {
        return HelloOutcome.networkFail();
      }
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
  /// - PC 勾选了"记住此手机"→ ack 带 token，落盘为常用电脑；
  /// - 否则是临时会话，只存内存，不落任何凭证。
  Future<HelloOutcome> pairWithCode(
    String ip,
    int port,
    String code,
  ) async {
    lastAuthMode = AuthMode.code;
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
    if (outcome.ok) return (ok: true, error: null);
    // 网络层失败且能判断两者不在同一网络 → 直说是网络问题，别让用户去猜电脑
    if (outcome.status == 'network_fail') {
      final net = await localNetSnapshot();
      final off = likelyOffLan(uri.host, net?.ips ?? const []);
      return (
        ok: false,
        error: off ? '连接失败，请确认手机和电脑连着同一个 Wi-Fi' : '连接失败',
      );
    }
    return (ok: false, error: _describe(outcome));
  }

  /// occupied 状态下用户点"接管电脑"
  Future<bool> takeover() async {
    final ok = await _tcp.takeover();
    if (ok && session != null) {
      // takeover 后身份仍为 primary（能走到这的必然是认证过的常用设备）
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

  /// 用户主动断开（绿灯）：bye 优雅告别 + 本进程不再自动重连。
  ///
  /// 同时作废本地的临时续连记录：主动断开 = 这次会话到此为止。
  /// 否则界面会因为"还有可连目标"而停在断开态、不回扫码引导页
  /// （临时设备降级来的会话尤其明显：它已经没有常用电脑记录了）。
  Future<void> userDisconnect() async {
    // 先置标记：bye 会同步广播 disconnected，避免那一帧排程出自动重连
    manualDisconnected = true;
    await _tcp.bye();
    await _storage.clearTransientSession();
  }

  /// PC 端托盘主动断开（owner_disconnect）时同样视为"用户意图的断开"，
  /// 并清掉本地临时续连记录（PC 侧那条记录也已经作废）
  void markDisconnectedByPc() {
    manualDisconnected = true;
    unawaited(_storage.clearTransientSession());
  }

  /// PC 端解绑本机：会话降级为临时设备（连接保持），删除本地落盘凭证。
  ///
  /// 与 forgetDevice 的区别：不发 bye、不清理内存会话——TCP 会话继续用于
  /// 打字粘贴，只是身份从常用设备变为临时设备；断线后不再自动重连（无凭证）。
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
        return '连接失败';
    }
  }

  void dispose() {
    _downgradeSub?.cancel();
    _promotedSub?.cancel();
    _identitySub?.cancel();
    _sessionUpdatedController.close();
    _tcp.dispose();
  }
}
