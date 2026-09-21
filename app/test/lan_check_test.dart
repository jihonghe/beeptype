import 'package:flutter_test/flutter_test.dart';
import 'package:beetype/services/lan_check.dart';
import 'package:beetype/services/retry_policy.dart';
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

  group('判断不了就不判断（同一个 Wi-Fi 下不能谎报"不在同一网络"）', () {
    test('取不到网卡信息（枚举失败）→ 不提示', () {
      expect(
        looksOffLan('192.168.31.7', null),
        isFalse,
        reason: '枚举网卡在安卓网络切换时确实会抛；那不等于"手机没有局域网"',
      );
    });

    test('目标地址未知 → 不提示', () {
      expect(
        looksOffLan(null, const LocalNet([(name: 'wlan0', ip: '192.168.31.20')])),
        isFalse,
      );
    });

    test('确实枚举到了网卡、目标不在任何网段 → 提示成立', () {
      expect(
        looksOffLan(
          '192.168.31.7',
          const LocalNet([(name: 'wlan0', ip: '192.168.1.20')]),
        ),
        isTrue,
      );
    });

    test('同一网段 → 不提示', () {
      expect(
        looksOffLan(
          '192.168.31.7',
          const LocalNet([(name: 'wlan0', ip: '192.168.31.20')]),
        ),
        isFalse,
      );
    });
  });

  group('本机是否有局域网（决定还要不要重试/广播）', () {
    test('只有移动数据（rmnet）→ 没有局域网：不重试、不广播', () {
      const net = LocalNet([(name: 'rmnet_data0', ip: '10.42.13.88')]);
      expect(net.hasLan, isFalse);
      expect(net.broadcastTargets(), {'255.255.255.255'});
    });

    test('连着 Wi-Fi → 有局域网', () {
      const net = LocalNet([(name: 'wlan0', ip: '192.168.31.20')]);
      expect(net.hasLan, isTrue);
      expect(net.broadcastTargets(), contains('192.168.31.255'));
    });

    test('手机开着热点（作为网关 ap0）→ 有局域网，且广播打到热点网段', () {
      // 这正是用户实测的场景：电脑连上手机热点后，手机必须能找到它
      const net = LocalNet([
        (name: 'rmnet_data0', ip: '10.42.13.88'),
        (name: 'ap0', ip: '192.168.43.1'),
      ]);
      expect(net.hasLan, isTrue);
      expect(net.broadcastTargets(), contains('192.168.43.255'));
    });

    test('认不出名字的网卡按"有局域网"处理（宁可多试一轮也不要停错）', () {
      const net = LocalNet([(name: 'weird0', ip: '172.20.10.2')]);
      expect(net.hasLan, isTrue);
      expect(net.broadcastTargets(), contains('172.20.10.255'));
    });

    test('网卡名分类', () {
      expect(isCellularInterface('rmnet_data0'), isTrue);
      expect(isCellularInterface('ccmni0'), isTrue);
      expect(isCellularInterface('wlan0'), isFalse);
      expect(isCellularInterface('ap0'), isFalse);
    });
  });

  group('判活超时（发现"假死"的连接）', () {
    final now = DateTime(2026, 9, 21, 12, 0, 0);
    const timeout = Duration(seconds: 6);

    test('前台静默超过窗口（6 秒）→ 判定已断（对端断网时 TCP 不会报错，只能这样发现）', () {
      expect(
        livenessExpired(
          lastInboundAt: now.subtract(const Duration(seconds: 7)),
          now: now,
          foreground: true,
          timeout: timeout,
        ),
        isTrue,
      );
    });

    test('窗口内还有回应 → 不算断', () {
      expect(
        livenessExpired(
          lastInboundAt: now.subtract(const Duration(seconds: 5)),
          now: now,
          foreground: true,
          timeout: timeout,
        ),
        isFalse,
      );
    });

    test('后台不判定（Doze 会延迟定时器，判了就是误伤）', () {
      expect(
        livenessExpired(
          lastInboundAt: now.subtract(const Duration(minutes: 10)),
          now: now,
          foreground: false,
          timeout: timeout,
        ),
        isFalse,
      );
    });

    test('还没有基准时刻（刚连上）→ 不判定', () {
      expect(
        livenessExpired(
          lastInboundAt: null,
          now: now,
          foreground: true,
          timeout: timeout,
        ),
        isFalse,
      );
    });
  });

  group('胶囊的词与按钮（状态归胶囊、该做什么归打字区）', () {
    test('只有"在忙"时才有词，其余状态一律不说话', () {
      expect(pillWord(busy: true), '连接中');
      expect(pillWord(busy: false), isNull, reason: '用户主动断开也不该出现状态词');
    });

    test('已连接 → 断开', () {
      expect(
        pillAction(connected: true, busy: false, hasTarget: true),
        PillAction.disconnect,
      );
    });

    test('在忙（连接中/重试中）→ 不显示按钮，避免重复操作', () {
      expect(
        pillAction(connected: false, busy: true, hasTarget: true),
        PillAction.none,
      );
      expect(
        pillAction(connected: false, busy: true, hasTarget: false),
        PillAction.none,
      );
    });

    test('连不上 / 主动断开 / 被占用（绑定手机）→ 连接', () {
      expect(
        pillAction(connected: false, busy: false, hasTarget: true),
        PillAction.connect,
      );
    });

    test('没绑定过电脑 → 不显示按钮（正文就是扫码引导页）', () {
      expect(
        pillAction(connected: false, busy: false, hasTarget: false),
        PillAction.none,
      );
    });
  });

  group('凭证被拒时：是不是"我那台电脑"在拒我', () {
    test('拒绝方就是记忆里那台电脑 → 真被解绑，该删绑定', () {
      expect(
        rejectionFromOurPc(responderId: 'pc-A', expectedId: 'pc-A'),
        isTrue,
      );
    });

    test('拒绝方是另一台设备（旧地址被别的设备占了）→ 保留绑定', () {
      expect(
        rejectionFromOurPc(responderId: 'pc-B', expectedId: 'pc-A'),
        isFalse,
        reason: '一台陌生产品不能让我们丢掉绑定',
      );
    });

    test('对方没报 device_id（旧版电脑）→ 按旧行为处理', () {
      expect(rejectionFromOurPc(responderId: null, expectedId: 'pc-A'), isTrue);
      expect(rejectionFromOurPc(responderId: '', expectedId: 'pc-A'), isTrue);
    });

    test('自己没存 device_id → 无从核对，按旧行为处理', () {
      expect(rejectionFromOurPc(responderId: 'pc-B', expectedId: null), isTrue);
    });
  });

  group('对端关闭连接算谁的问题（锁屏被收尸 / 自己换网络都不能赖电脑）', () {
    test('我们当时还在正常通信、网络也没刚变过 → 算电脑侧', () {
      expect(
        peerCloseIsReceiverSide(
          peerClosed: true,
          weWereTalking: true,
          netJustChanged: false,
        ),
        isTrue,
      );
    });

    test('我们自己静默了（锁屏/后台被挂起，PC 收尸）→ 不算电脑侧', () {
      expect(
        peerCloseIsReceiverSide(
          peerClosed: true,
          weWereTalking: false,
          netJustChanged: false,
        ),
        isFalse,
        reason: '是我们自己没发心跳，锅在手机侧，记录要留着等窗口期内续连',
      );
    });

    test('手机自己刚换过网络（比如开热点）→ 不算电脑侧', () {
      expect(
        peerCloseIsReceiverSide(
          peerClosed: true,
          weWereTalking: true,
          netJustChanged: true,
        ),
        isFalse,
      );
    });

    test('压根不是对端关闭（静默超时）→ 不算电脑侧', () {
      expect(
        peerCloseIsReceiverSide(
          peerClosed: false,
          weWereTalking: false,
          netJustChanged: false,
        ),
        isFalse,
      );
    });
  });

  group('被电脑拒绝时的说法（不能谎称"被解绑"）', () {
    test('绑定手机被拒 = 真被解绑', () {
      expect(
        credentialRejectedMessage(bound: true),
        '电脑已解除与你的绑定，请重新扫码',
      );
    });

    test('临时接入被拒 = 电脑忘了它（PC 服务重启）或窗口过期', () {
      expect(
        credentialRejectedMessage(bound: false),
        '连接已失效，请重新扫码',
      );
    });
  });

  group('重试节奏（快速试一阵就停手）', () {
    test('一轮约 1 分钟就停：不再有"永远每 10 秒一次"', () {
      expect(retryEpisodeMs, greaterThan(60000));
      expect(retryEpisodeMs, lessThan(70000));
      expect(retryAllowed(0), isTrue);
      expect(retryAllowed(retryDelaysMs.length - 1), isTrue);
      expect(retryAllowed(retryDelaysMs.length), isFalse);
      expect(retryAllowed(99), isFalse, reason: '试满就该停手，交给用户点重新连接');
    });

    test('先快后慢：第 0 次 1 秒、第 4 次起 10 秒', () {
      expect(retryDelayMs(0), 1000);
      expect(retryDelayMs(1), 2000);
      expect(retryDelayMs(2), 4000);
      expect(retryDelayMs(3), 8000);
      expect(retryDelayMs(4), 10000);
      expect(retryDelayMs(100), 10000);
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

  group('心跳里的权威信息（pong.session / resume_expires_in）', () {
    test('临时接入的窗口时长会被带回来（手机据此刷新本地到期时间）', () {
      const info = HeartbeatInfo(session: 'transient', resumeExpiresIn: 300);
      expect(info.session, 'transient');
      expect(info.resumeExpiresIn, 300);
    });

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
