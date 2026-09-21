import 'dart:convert';
import 'dart:math';
import 'package:flutter/services.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// 一台已绑定的输出设备（持长期 token 的常用电脑）。
///
/// 临时会话（这台电脑已有别的常用设备、自己只是扫码接入）不落盘，仅存内存。
class OutputDevice {
  final String deviceId;
  String name;
  final String deviceType; // 目前固定 "pc"
  String ip;
  int port;
  final String token;
  Map<String, dynamic> capabilities;
  int lastConnectedMs;

  OutputDevice({
    required this.deviceId,
    required this.name,
    required this.deviceType,
    required this.ip,
    required this.port,
    required this.token,
    required this.capabilities,
    required this.lastConnectedMs,
  });

  factory OutputDevice.fromJson(Map<String, dynamic> j) => OutputDevice(
        deviceId: j['device_id'] as String? ?? '',
        name: j['name'] as String? ?? '我的电脑',
        deviceType: j['device_type'] as String? ?? 'pc',
        ip: j['ip'] as String? ?? '',
        port: (j['port'] as num?)?.toInt() ?? 8765,
        token: j['token'] as String? ?? '',
        capabilities:
            (j['capabilities'] as Map?)?.cast<String, dynamic>() ?? const {},
        lastConnectedMs: (j['last_connected_ms'] as num?)?.toInt() ?? 0,
      );

  Map<String, dynamic> toJson() => {
        'device_id': deviceId,
        'name': name,
        'device_type': deviceType,
        'ip': ip,
        'port': port,
        'token': token,
        'capabilities': capabilities,
        'last_connected_ms': lastConnectedMs,
      };
}

/// 一条**临时会话**的续连记录（与"已绑定电脑"分开存，不混进常用列表）。
///
/// 临时设备本身不落长期凭证：只有在 PC 侧开启了"断线后自动续连"时，才会在配对
/// 成功时拿到续连凭证，并在窗口期内凭它免扫码回到同一台电脑；窗口过期或 PC 侧
/// 作废（重启/主人断开）后即失效，需要重新扫码。
class TransientSession {
  final String deviceId;
  final String name;
  final String ip;
  final int port;
  final String token;
  final int expiresAtMs;

  TransientSession({
    required this.deviceId,
    required this.name,
    required this.ip,
    required this.port,
    required this.token,
    required this.expiresAtMs,
  });

  bool get isExpired =>
      DateTime.now().millisecondsSinceEpoch >= expiresAtMs;

  TransientSession copyWith({int? expiresAtMs}) => TransientSession(
        deviceId: deviceId,
        name: name,
        ip: ip,
        port: port,
        token: token,
        expiresAtMs: expiresAtMs ?? this.expiresAtMs,
      );

  factory TransientSession.fromJson(Map<String, dynamic> j) => TransientSession(
        deviceId: j['device_id'] as String? ?? '',
        name: j['name'] as String? ?? '电脑',
        ip: j['ip'] as String? ?? '',
        port: (j['port'] as num?)?.toInt() ?? 8765,
        token: j['token'] as String? ?? '',
        expiresAtMs: (j['expires_at_ms'] as num?)?.toInt() ?? 0,
      );

  Map<String, dynamic> toJson() => {
        'device_id': deviceId,
        'name': name,
        'ip': ip,
        'port': port,
        'token': token,
        'expires_at_ms': expiresAtMs,
      };
}

/// 存储服务：手机身份 + 已绑定电脑列表。
///
/// 单常用设备模型体现在 PC 端（一台 PC 只记一部常用设备）；手机端可记住多台
/// 电脑（公司/家里），v1 自动连接最近使用的一台（[primaryDevice]）。
class StorageService {
  static const _keyClientId = 'client_id';
  static const _keyDevices = 'output_devices';
  static const _keyDeviceName = 'device_name';
  static const _keyTransient = 'transient_session';

  // 旧版本（配对码模型）键名：升级后一次性清除，旧配对在新协议已失效。
  static const _legacyKeys = [
    'pc_ip',
    'pc_port',
    'pairing_code',
    'is_paired',
  ];

  late SharedPreferences _prefs;

  /// 初始化（含旧版本数据迁移与真实设备名同步）
  Future<void> init() async {
    _prefs = await SharedPreferences.getInstance();
    await _migrateLegacy();
    await syncDeviceName();
  }

  /// 把假默认设备名替换为系统真实设备名（蓝牙/投屏同款）。每次启动及切回前台时
  /// 调用，用户在系统设置里改名后 PC 端显示也跟着更新
  Future<void> syncDeviceName() async {
    try {
      final real = await const MethodChannel('beeptype/device')
          .invokeMethod<String>('getDeviceName');
      final name = real?.trim() ?? '';
      if (name.isEmpty || name.toLowerCase().startsWith('android')) return;
      await _prefs.setString(_keyDeviceName, name);
    } catch (_) {
      // 通道不可用（非安卓平台/引擎未就绪）时维持现值
    }
  }

  /// 旧键存在即清除，且不做任何转换（配对码无法兑换新 token，必须重新绑定）
  Future<void> _migrateLegacy() async {
    var hasLegacy = false;
    for (final k in _legacyKeys) {
      if (_prefs.containsKey(k)) {
        hasLegacy = true;
        await _prefs.remove(k);
      }
    }
    if (hasLegacy) {
      // device_name 旧默认值是 'Android Device'，保留键即可，不清。
    }
  }

  /// 手机稳定身份（首启生成 UUIDv4），跨重连不变。
  String get clientId {
    var id = _prefs.getString(_keyClientId);
    if (id == null || id.isEmpty) {
      id = _newUuidV4();
      _prefs.setString(_keyClientId, id);
    }
    return id;
  }

  /// 手机展示名（hello.device_name 带给 PC）
  String getDeviceName() =>
      _prefs.getString(_keyDeviceName) ?? 'Android 手机';

  /// 全部已绑定电脑，按最近使用倒序
  List<OutputDevice> get devices {
    final raw = _prefs.getString(_keyDevices);
    if (raw == null || raw.isEmpty) return const [];
    try {
      final list = (jsonDecode(raw) as List)
          .whereType<Map>()
          .map((m) => OutputDevice.fromJson(m.cast<String, dynamic>()))
          .toList();
      list.sort((a, b) => b.lastConnectedMs.compareTo(a.lastConnectedMs));
      return list;
    } catch (_) {
      return const [];
    }
  }

  /// v1 自动连接目标：最近使用的一台；没有绑定电脑时为 null
  OutputDevice? get primaryDevice {
    final list = devices;
    return list.isEmpty ? null : list.first;
  }

  bool get hasDevices => devices.isNotEmpty;

  OutputDevice? findDevice(String deviceId) {
    for (final d in devices) {
      if (d.deviceId == deviceId) return d;
    }
    return null;
  }

  /// 插入或更新一台绑定电脑（以 device_id 去重）
  Future<void> upsertDevice(OutputDevice device) async {
    final list = devices.where((d) => d.deviceId != device.deviceId).toList();
    list.add(device);
    list.sort((a, b) => b.lastConnectedMs.compareTo(a.lastConnectedMs));
    await _prefs.setString(
      _keyDevices,
      jsonEncode(list.map((d) => d.toJson()).toList()),
    );
  }

  /// 连接成功后刷新 IP/名称/能力/最近使用时间
  Future<void> touchDevice(
    String deviceId, {
    String? ip,
    int? port,
    String? name,
    Map<String, dynamic>? capabilities,
  }) async {
    final list = devices;
    for (final d in list) {
      if (d.deviceId == deviceId) {
        if (ip != null && ip.isNotEmpty) d.ip = ip;
        if (port != null) d.port = port;
        if (name != null && name.isNotEmpty) d.name = name;
        if (capabilities != null) d.capabilities = capabilities;
        d.lastConnectedMs = DateTime.now().millisecondsSinceEpoch;
      }
    }
    list.sort((a, b) => b.lastConnectedMs.compareTo(a.lastConnectedMs));
    await _prefs.setString(
      _keyDevices,
      jsonEncode(list.map((d) => d.toJson()).toList()),
    );
  }

  Future<void> removeDevice(String deviceId) async {
    final list = devices.where((d) => d.deviceId != deviceId).toList();
    await _prefs.setString(
      _keyDevices,
      jsonEncode(list.map((d) => d.toJson()).toList()),
    );
  }

  // ---- 临时会话的续连记录（与常用电脑列表分开，互不影响） ----

  /// 当前临时会话记录；没有则 null（过期与否由调用方判断，便于给出准确提示）
  TransientSession? get transientSession {
    final raw = _prefs.getString(_keyTransient);
    if (raw == null || raw.isEmpty) return null;
    try {
      return TransientSession.fromJson(
          jsonDecode(raw) as Map<String, dynamic>);
    } catch (_) {
      return null;
    }
  }

  Future<void> saveTransientSession(TransientSession s) =>
      _prefs.setString(_keyTransient, jsonEncode(s.toJson()));

  /// 续连成功后刷新到期时间（滑动窗口，由 PC 侧给的窗口时长决定）
  Future<void> touchTransientSession({int? expiresInSec}) async {
    final cur = transientSession;
    if (cur == null) return;
    final next = expiresInSec == null
        ? cur
        : cur.copyWith(
            expiresAtMs: DateTime.now().millisecondsSinceEpoch +
                expiresInSec * 1000);
    await saveTransientSession(next);
  }

  Future<void> clearTransientSession() => _prefs.remove(_keyTransient);

  /// RFC 4122 v4 UUID，密码学随机源
  static String _newUuidV4() {
    final rnd = Random.secure();
    final b = List<int>.generate(16, (_) => rnd.nextInt(256));
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    String hex(int x) => x.toRadixString(16).padLeft(2, '0');
    final s = b.map(hex).join();
    return '${s.substring(0, 8)}-${s.substring(8, 12)}-${s.substring(12, 16)}'
        '-${s.substring(16, 20)}-${s.substring(20)}';
  }
}
