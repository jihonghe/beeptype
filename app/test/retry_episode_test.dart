import 'package:flutter_test/flutter_test.dart';
import 'package:beeptype/services/retry_episode.dart';

/// 「一轮快速重试」状态机的完整护栏。
///
/// 这台状态机是为了堵住三次真实事故而抽出来的，所以测试也照那三条来：
/// ① 倒计时归零后重试还在跑（一轮的真实时长被按"次数"算歪了）；
/// ② 一轮结束后又冒出一轮（"是否开新的一轮"判断错，任何后续触发都开新轮）；
/// ③ 到点了却没人来问一句，于是界面一直停在"在忙"（用户实测：已连接时冒出
///    「连接中 · N 秒」+「断开」，倒计时结束后那行字还赖着不走）。
void main() {
  group('一轮的生命周期', () {
    test('开一轮：倒计时当场可见（不留"连接中却没有秒数"的空白）', () {
      final e = RetryEpisode();
      expect(e.isActive(1000), isFalse);
      expect(e.remainingSeconds(1000), isNull, reason: '没在跑时不显示倒计时');
      e.start(1000);
      expect(e.isActive(1000), isTrue);
      expect(e.deadlineMs, 31000, reason: '开轮那一刻就定下 30 秒截止');
      expect(e.remainingSeconds(1000), 30, reason: '开轮即可见');
    });

    test('每次间隔 2 秒（最后一次压进剩余时间）', () {
      final e = RetryEpisode();
      e.start(0);
      expect(e.deadlineMs, 30000);
      expect(e.nextDelayMs(0), 2000);
      expect(e.nextDelayMs(2000), 2000);
      expect(e.nextDelayMs(4000), 2000);
      expect(e.attempt, 3);
    });

    test('到点即结束：shouldSchedule 变 false，且不再算"进行中"', () {
      final e = RetryEpisode();
      e.start(0);
      e.nextDelayMs(0);
      expect(e.shouldSchedule(29999), isTrue);
      expect(e.shouldSchedule(30000), isFalse);
      expect(e.isActive(30000), isFalse);
    });

    test('到点即"不在忙"——不依赖任何人调用 shouldSchedule（事故③的护栏）', () {
      final e = RetryEpisode();
      e.start(0);
      // 模拟"某条路径开了轮、之后没人来问 shouldSchedule"（已连接时从扫码页返回）
      expect(e.isActive(29999), isTrue);
      expect(e.remainingSeconds(29999), 1);
      expect(e.isActive(30000), isFalse, reason: '到点就是到点，与有没有被问过无关');
      expect(e.remainingSeconds(30000), isNull,
          reason: '过点后不再显示倒计时，界面也就不会再写「连接中」');
      expect(e.remainingSeconds(60000), isNull);
    });

    test('最后一次等待压进剩余时间，避免到点后又多等一次', () {
      final e = RetryEpisode();
      e.start(0);
      expect(e.nextDelayMs(29500), 500, reason: '只剩 500ms 就不该再等 2 秒');
    });

    test('兜底上限：一轮最多排 maxAttempts 次重试（时长仍是权威）', () {
      final e = RetryEpisode(maxAttempts: 3);
      e.start(0);
      var now = 0;
      for (var i = 0; i < 3; i++) {
        expect(e.shouldSchedule(now), isTrue);
        now += e.nextDelayMs(now);
      }
      expect(e.shouldSchedule(now), isFalse, reason: '保险丝烧了：停手');
      expect(e.isActive(now), isFalse);
    });

    test('结束一轮：清空状态，倒计时也不再显示', () {
      final e = RetryEpisode();
      e.start(0);
      e.nextDelayMs(0);
      e.end();
      expect(e.isActive(1000), isFalse);
      expect(e.deadlineMs, isNull);
      expect(e.remainingSeconds(1000), isNull);
      expect(e.shouldSchedule(1000), isFalse, reason: '一轮结束后不允许自己接着跑');
    });

    test('再次触发 = 全新一轮（给足 30 秒、次数归零）', () {
      final e = RetryEpisode();
      e.start(0);
      e.nextDelayMs(0);
      e.nextDelayMs(2000);
      e.end();
      e.start(100000);
      expect(e.isActive(100000), isTrue);
      expect(e.attempt, 0);
      expect(e.deadlineMs, 130000, reason: '再触发 = 全新一轮的 30 秒');
      expect(e.nextDelayMs(100000), 2000);
    });

    test('没在跑的时候不会排任何重试（触发才开轮）', () {
      final e = RetryEpisode();
      expect(e.shouldSchedule(0), isFalse);
      expect(e.nextDelayMs(0), 2000, reason: '取等待只是计算，不代表开了一轮');
      expect(e.isActive(0), isFalse, reason: '未开轮时取等待不应把它变成"进行中"');
    });
  });
}
