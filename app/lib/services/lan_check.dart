/// 本机网络快照与「是否可能在同一局域网」的判断。
///
/// 只用于两件用户可见的事，**不做任何断言**：
/// 1. 连接失败时决定要不要提示"请确认手机和电脑连着同一个 Wi-Fi"；
/// 2. 决定还有没有必要继续重试（本产品只在同一局域网下能工作，只有移动数据时
///    连上去也无从谈起，重试与广播都是白耗电）。
///
/// 顺带也用于算 UDP 广播目标：**枚举所有 IPv4 网卡**，每个网段各发一份定向广播。
/// 只看"Wi-Fi 客户端地址"是不够的——手机开热点时它作为网关（ap0/swlan0）并没有
/// 作为客户端连着 Wi-Fi，那种情况下全局广播会从移动数据出去，永远到不了热点网段，
/// 于是"电脑已经连上手机热点，手机却找不到它"（用户实测复现过）。
///
/// 读网卡信息不需要任何权限（读 Wi-Fi 名称才需要位置权限，所以不用）。
library;

import 'dart:io';

/// 明显是移动数据的网卡名（Android/iOS 上的运营商接口）
const _cellularPrefixes = ['rmnet', 'ccmni', 'pdp', 'wwan', 'seth'];

/// 本机网络快照：网卡名 + IPv4 地址。
class LocalNet {
  final List<({String name, String ip})> entries;

  const LocalNet(this.entries);

  List<String> get ips => [for (final e in entries) e.ip];

  /// 是否存在**非移动数据**的网卡（Wi-Fi、手机热点、USB/以太网共享都算）。
  ///
  /// 网卡名认不出来时按"有局域网"处理：宁可多试一轮，也不要因为认不出名字
  /// 就停止重试（那样会真的连不回来）。
  bool get hasLan => entries.any((e) => !isCellularInterface(e.name));

  /// UDP 广播目标：每个**局域网**网段的定向广播 + 全局广播兜底。
  /// 移动数据网段（运营商接口）不算——往那里广播没有意义。
  Set<String> broadcastTargets() {
    final targets = <String>{'255.255.255.255'};
    for (final e in entries) {
      if (isCellularInterface(e.name)) continue;
      final b = subnetBroadcast24(e.ip);
      if (b != null) targets.add(b);
    }
    return targets;
  }
}

/// 网卡名是否是移动数据接口
bool isCellularInterface(String name) =>
    _cellularPrefixes.any((p) => name.startsWith(p));

/// 枚举本机 IPv4 网卡（不含回环）。取不到时返回 null——
/// 调用方据此按"不确定"处理（继续重试），而不是误判成"没有局域网"。
Future<LocalNet?> localNetSnapshot() async {
  try {
    final ifaces = await NetworkInterface.list(
      type: InternetAddressType.IPv4,
      includeLoopback: false,
    );
    return LocalNet([
      for (final i in ifaces)
        for (final a in i.addresses) (name: i.name, ip: a.address),
    ]);
  } catch (_) {
    return null;
  }
}

/// 目标地址是否**看起来**不在本机任何一个局域网网段内。
///
/// 与 [likelyOffLan] 的区别：这里把"取不到网卡信息"（[net] 为 null，枚举抛异常）
/// 明确当成**判断不了**——返回 false、不给提示。枚举网卡在安卓网络切换/热点共享
/// 这类时刻确实会抛，若把它当成"没有局域网"，就会在同一个 Wi-Fi 下谎报
/// "请确认手机和电脑连着同一个 Wi-Fi"（用户实测踩到过）。
bool looksOffLan(String? targetIp, LocalNet? net) {
  if (targetIp == null || net == null) return false;
  return likelyOffLan(targetIp, net.ips);
}

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

/// 网段的定向广播地址（按 /24 推算）；不是合法 IPv4 时返回 null。
String? subnetBroadcast24(String ip) {
  final p = _prefix24(ip);
  return p == null ? null : '$p.255';
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
