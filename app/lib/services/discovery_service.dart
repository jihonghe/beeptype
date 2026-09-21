import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'package:crypto/crypto.dart';
import 'package:flutter/foundation.dart';

import 'lan_check.dart';

/// PC 发现应答中的设备描述
class DiscoveredDevice {
  final String deviceId;
  final String name;
  final String deviceType;
  final String ip;
  final int port;
  final bool busy;
  final Map<String, dynamic> capabilities;

  DiscoveredDevice({
    required this.deviceId,
    required this.name,
    required this.deviceType,
    required this.ip,
    required this.port,
    required this.busy,
    required this.capabilities,
  });
}

/// UDP 局域网发现。
///
/// 协议与 PC 端 discovery.rs 对齐：
/// - 请求  vl_disc_req {protocol:1, client_id, proof=SHA256(token)}
/// - 应答  vl_disc_resp{protocol:1, device_id, device_name, device_type,
///                       port, busy, capabilities}
///
/// PC 端凭 (client_id, proof) 门控：不是该机的常用设备，一律静默。
/// 因此本服务收到的应答必然属于【持有该 token 的那台电脑】，无需再猜。
class DiscoveryService {
  static const int _protocolVersion = 1;
  static const int _rounds = 3;
  static const Duration _roundInterval = Duration(milliseconds: 250);
  static const Duration _listenAfter = Duration(milliseconds: 450);

  /// 发现持有 [token] 对应凭证的电脑。
  ///
  /// [port] 为约定端口（默认 8765，与 PC TCP 同号）。
  /// 总预算约 (rounds-1)*300ms + 500ms ≈ 1.1s。
  /// [expectDeviceId]：只接受这台设备的应答。
  ///
  /// 应答方虽然必须"认得这张凭证"才可能应答（证明它握有同一个 token 哈希），但
  /// **陌生设备可以盲答**（不判断、见广播就回自己的地址）——那会把手机引到一个
  /// 错的地址，甚至把凭证送给它。所以手机要核对 `device_id` 是不是记忆里那台电脑。
  Future<List<DiscoveredDevice>> discover({
    required String clientId,
    required String token,
    int port = 8765,
    String? expectDeviceId,
  }) async {
    final proof = sha256.convert(utf8.encode(token)).toString();
    final payload = utf8.encode(jsonEncode({
      'type': 'vl_disc_req',
      'protocol': _protocolVersion,
      'client_id': clientId,
      'proof': proof,
    }));

    final RawDatagramSocket socket;
    try {
      socket = await RawDatagramSocket.bind(InternetAddress.anyIPv4, 0);
      socket.broadcastEnabled = true;
    } catch (_) {
      return const [];
    }

    final targets = await _broadcastTargets(port);
    // 没有局域网网卡（例如只有移动数据）：广播无处可去，立即返回，不做无谓等待
    if (targets.isEmpty) return const [];
    final found = <String, DiscoveredDevice>{}; // device_id 去重

    final completer = Completer<void>();
    late final StreamSubscription sub;
    sub = socket.listen((event) {
      if (event != RawSocketEvent.read) return;
      final dg = socket.receive();
      if (dg == null) return;
      final dev = _parseResponse(dg.data, dg.address.address);
      if (dev == null) return;
      if (expectDeviceId != null &&
          expectDeviceId.isNotEmpty &&
          dev.deviceId != expectDeviceId) {
        debugPrint('VL_DISC 忽略一台陌生设备的应答（device_id 不匹配）');
        return;
      }
      if (!found.containsKey(dev.deviceId)) {
        found[dev.deviceId] = dev;
      }
    }, onError: (Object _) {
      if (!completer.isCompleted) completer.complete();
    });

    // 多轮广播（UDP 不可靠，靠轮次兜底丢包）
    for (var i = 0; i < _rounds; i++) {
      for (final addr in targets) {
        try {
          socket.send(payload, addr, port);
        } catch (_) {}
      }
      if (i < _rounds - 1) {
        await Future<void>.delayed(_roundInterval);
      }
    }
    // 最后一轮后再收一会儿尾包
    Timer(_listenAfter, () {
      if (!completer.isCompleted) completer.complete();
    });

    try {
      await completer.future
          .timeout(_roundInterval * _rounds + _listenAfter);
    } catch (_) {}

    await sub.cancel();
    socket.close();
    return found.values.toList();
  }

  /// 广播目标：有限广播 + 当前 WiFi 子网定向广播（部分 ROM 会丢弃 255.255.255.255）
  /// 广播目标：**枚举本机所有 IPv4 网卡**，每个网段各发一份定向广播。
  ///
  /// 不能用"Wi-Fi 客户端地址"代表全部：手机开热点时它是网关（ap0/swlan0），
  /// 自己并没有作为客户端连着 Wi-Fi；那种情况下只有全局广播可用，而全局广播会从
  /// 默认路由（移动数据）出去，永远到不了热点网段——"电脑明明连上手机热点了，
  /// 手机却找不到电脑"就是这么来的（用户实测复现）。
  ///
  /// 没有任何局域网网卡时直接返回空集：广播只在同一局域网内有意义，
  /// 在移动数据上发只是白耗电（调用方据此也不做等待）。
  Future<Set<InternetAddress>> _broadcastTargets(int port) async {
    final targets = <InternetAddress>{};
    final net = await localNetSnapshot();
    if (net == null || !net.hasLan) return targets;
    for (final bcast in net.broadcastTargets()) {
      targets.add(InternetAddress(bcast));
    }
    debugPrint('VL_DISC 本机网卡：'
        '${net.entries.map((e) => '${e.name}=${e.ip}').join(', ')}'
        ' → 广播目标：${targets.map((t) => t.address).join(', ')}');
    return targets;
  }

  /// 只处理常见的 /24 家用网段（192.168.x.x / 10.x.x.x / 172.16-31.x.x），
  /// 掩码不一致则不推算，返回 null。

  DiscoveredDevice? _parseResponse(List<int> data, String srcIp) {
    try {
      final j = jsonDecode(utf8.decode(data)) as Map<String, dynamic>;
      if (j['type'] != 'vl_disc_resp') return null;
      if ((j['protocol'] as num?)?.toInt() != _protocolVersion) return null;
      final deviceId = j['device_id'] as String?;
      if (deviceId == null || deviceId.isEmpty) return null;
      return DiscoveredDevice(
        deviceId: deviceId,
        name: (j['device_name'] as String?) ?? '我的电脑',
        deviceType: (j['device_type'] as String?) ?? 'pc',
        ip: srcIp, // 以包来源为准，NAT/多网卡下最可靠
        port: (j['port'] as num?)?.toInt() ?? 8765,
        busy: j['busy'] as bool? ?? false,
        capabilities:
            (j['capabilities'] as Map?)?.cast<String, dynamic>() ?? const {},
      );
    } catch (_) {
      return null;
    }
  }
}
