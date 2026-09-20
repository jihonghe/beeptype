/// 「手机和电脑是否可能在同一局域网」——只用于连接失败时决定要不要提示
/// "请确认手机和电脑连着同一个 Wi-Fi"，**不做任何断言**。
///
/// 判据刻意保守：把目标地址与手机当前所有 IPv4 地址按 /24 比一比。
/// - 目标不在任何一个网段里 → 两者不可能在同一局域网（手机在用移动数据、
///   或连了另一个 Wi-Fi），提示成立；
/// - 网段相同 → 什么都不说：私网地址段到处复用（192.168.1.x 几乎是路由器默认值），
///   同一个网段既可能真的同网、也可能只是撞段；而且公共 Wi-Fi 常禁止设备互访，
///   那种"同网也连不上"根本不该提示用户去检查 Wi-Fi。
///
/// 读网卡地址不需要任何权限（读 Wi-Fi 名称才需要位置权限，所以不用）。
library;

import 'dart:io';

/// 目标地址是否**看起来**不在本机任何一个局域网网段内。
///
/// [localIps] 为本机当前 IPv4 地址；目标地址无法解析时返回 false（判断不了就别说）。
bool likelyOffLan(String targetIp, Iterable<String> localIps) {
  final target = _prefix24(targetIp);
  if (target == null) return false;
  for (final ip in localIps) {
    if (_prefix24(ip) == target) return false;
  }
  return true;
}

/// 本机当前所有 IPv4 地址（不含回环）。取不到时返回空列表。
Future<List<String>> localIpv4Addresses() async {
  try {
    final ifaces = await NetworkInterface.list(
      type: InternetAddressType.IPv4,
      includeLoopback: false,
    );
    return [
      for (final i in ifaces)
        for (final a in i.addresses) a.address,
    ];
  } catch (_) {
    return const [];
  }
}

/// 取 IPv4 的前三段（/24）。不是合法 IPv4 时返回 null。
String? _prefix24(String ip) {
  final parts = ip.split('.');
  if (parts.length != 4) return null;
  for (final p in parts) {
    final n = int.tryParse(p);
    if (n == null || n < 0 || n > 255) return null;
  }
  return '${parts[0]}.${parts[1]}.${parts[2]}';
}
