import 'package:flutter_test/flutter_test.dart';
import 'package:beetype/services/lan_check.dart';
import 'package:beetype/services/tcp_service.dart';

/// 「不在同一个局域网」判断的护栏。
///
/// 这条判断只影响一句提示（"请确认手机和电脑连着同一个 Wi-Fi"），
/// 所以设计上**只说得清的情况才提示**：目标地址不在手机任何网段里才成立。
void main() {
  group('likelyOffLan（是否判断为不在同一局域网）', () {
    test('手机用移动数据（只有运营商地址）→ 提示成立', () {
      expect(likelyOffLan('192.168.31.7', ['10.42.13.88']), isTrue);
    });

    test('手机换了另一个 Wi-Fi（网段不同）→ 提示成立', () {
      expect(likelyOffLan('192.168.31.7', ['192.168.1.20']), isTrue);
    });

    test('同一个 Wi-Fi → 不提示（公共 Wi-Fi 禁止设备互访也不该甩锅给用户）', () {
      expect(likelyOffLan('192.168.31.7', ['192.168.31.20']), isFalse);
    });

    test('手机自己开了热点、电脑连在热点上 → 不提示', () {
      expect(likelyOffLan('192.168.43.7', ['192.168.43.1']), isFalse);
    });

    test('多网卡（Wi-Fi + VPN/热点）任一命中即视为同网', () {
      expect(
        likelyOffLan('192.168.31.7', ['10.8.0.2', '192.168.31.20']),
        isFalse,
      );
    });

    test('手机一个 IPv4 都没有 → 提示成立（连不上任何局域网节点）', () {
      expect(likelyOffLan('192.168.31.7', const []), isTrue);
    });

    test('目标地址不合法 → 判断不了就不提示', () {
      expect(likelyOffLan('not-an-ip', ['192.168.31.20']), isFalse);
      expect(likelyOffLan('192.168.31', ['192.168.31.20']), isFalse);
      expect(likelyOffLan('192.168.31.999', ['192.168.31.20']), isFalse);
    });

    test('同网段撞车（不同网络恰好都用 192.168.1.x）→ 不提示，宁可沉默', () {
      expect(likelyOffLan('192.168.1.7', ['192.168.1.20']), isFalse);
    });
  });

  group('凭证被拒 vs 被限流（决定要不要删掉绑定）', () {
    test('电脑明确拒绝 → 删除本地凭证并回扫码页', () {
      final o = HelloOutcome.fromAck({'ok': false, 'status': 'unauthorized'});
      expect(o.isUnauthorized, isTrue, reason: '凭证确实失效了，该删');
      expect(o.isRateLimited, isFalse);
    });

    test('只是被限流（等 60 秒再来）→ 保留凭证，只等一会儿', () {
      final o = HelloOutcome.fromAck({'ok': false, 'status': 'cooldown'});
      expect(o.isRateLimited, isTrue);
      expect(
        o.isUnauthorized,
        isFalse,
        reason: '限流是临时的，删绑定等于"一次限流换一台好设备重扫"',
      );
    });

    test('网络层失败 → 既不删凭证也不算限流', () {
      final o = HelloOutcome.networkFail();
      expect(o.isUnauthorized, isFalse);
      expect(o.isRateLimited, isFalse);
    });
  });

  group('心跳里的权威身份（pong.session）', () {
    test('pong 不带身份时解出的 session 为空（老电脑 / 不持席）', () {
      final msg = TcpMessage.decode('{"type":"pong"}\n');
      expect(msg, isNotNull);
      expect(msg!.data['session'], isNull);
    });

    test('pong 带身份时原样解出', () {
      final msg = TcpMessage.decode('{"type":"pong","session":"transient"}\n');
      expect(msg!.data['session'], 'transient');
    });
  });
}
