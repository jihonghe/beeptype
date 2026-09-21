import 'package:flutter_test/flutter_test.dart';
import 'package:beetype/services/retry_episode.dart';

/// 「一轮快速重试」状态机的完整护栏。
///
/// 这台状态机是为了堵住两次真实事故而抽出来的，所以测试也照那两条来：
/// ① 倒计时归零后重试还在跑（一轮的真实时长被按"次数"算歪了）；
/// ② 一轮结束后又冒出一轮（"是否开新的一轮"判断错，任何后续触发都开新轮）。
void main() {
  group('一轮的生命周期', () {
    test('开一轮：进行中、还没定截止（所以还不显示倒计时）', () {
      final e = RetryEpisode();
      expect(e.active, isFalse);
      expect(e.remainingSeconds(1000), isNull);
      e.start(1000);
      expect(e.active, isTrue);
      expect(e.deadlineMs, isNull);
      expect(e.remainingSeconds(1000), isNull, reason: '还没开始重试就不该有倒计时');
    });

    test('排第一次重试时定下 30 秒的截止，此后每次间隔 2 秒', () {
      final e = RetryEpisode();
      e.start(0);
      expect(e.nextDelayMs(0), 2000);
      expect(e.deadlineMs, 30000);
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
      expect(e.active, isFalse);
    });

    test('最后一次等待压进剩余时间，避免到点后又多等一次', () {
      final e = RetryEpisode();
      e.start(0);
      e.nextDelayMs(0); // 定下 30 秒截止
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
      expect(e.active, isFalse);
    });

    test('结束一轮：清空状态，倒计时也不再显示', () {
      final e = RetryEpisode();
      e.start(0);
      e.nextDelayMs(0);
      e.end();
      expect(e.active, isFalse);
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
      expect(e.active, isTrue);
      expect(e.attempt, 0);
      expect(e.deadlineMs, isNull);
      expect(e.nextDelayMs(100000), 2000);
      expect(e.deadlineMs, 130000);
    });

    test('没在跑的时候不会排任何重试（触发才开轮）', () {
      final e = RetryEpisode();
      expect(e.shouldSchedule(0), isFalse);
      expect(e.nextDelayMs(0), 2000, reason: '取等待只是计算，不代表开了一轮');
      expect(e.active, isFalse, reason: '未开轮时取等待不应把它变成"进行中"');
    });
  });
}
