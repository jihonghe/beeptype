import 'package:flutter_test/flutter_test.dart';

import 'package:beeptype/services/voice_send_policy.dart';

/// "松手才发"的护栏。核心两件事：
///   ① 录音中一个字都不发（中途提交多少次都不发）；
///   ② 松手后**该等的才等**——还有没提交的文字就等它，没有就立刻发。
///
/// 真机依据（2026-09-23 诊断）：按住语音键期间麦克风一直被占用（停顿 5 秒也不放），
/// 松手立刻释放；输入法最后一段文本可能在松手后 0.5~1 秒才提交。
void main() {
  bool flush({
    bool recAvailable = true,
    bool recording = false,
    int textLength = 10,
    bool composingActive = false,
    bool voiceSession = true,
    int sinceLastTextChangeMs = 1000,
  }) =>
      shouldFlush(
        recAvailable: recAvailable,
        recording: recording,
        textLength: textLength,
        composingActive: composingActive,
        voiceSession: voiceSession,
        sinceLastTextChangeMs: sinceLastTextChangeMs,
      );

  test('录音中：中途提交多少次都不发（碎片问题的正解）', () {
    expect(flush(recording: true, sinceLastTextChangeMs: 9999), isFalse);
    expect(
        flush(recording: true, composingActive: true, sinceLastTextChangeMs: 9999),
        isFalse);
  });

  test('松手 + 没有待提交文字 ⇒ 80ms 合并窗口后立刻发（最快路径）', () {
    expect(flush(sinceLastTextChangeMs: kVoiceSettleMs), isTrue);
    expect(flush(sinceLastTextChangeMs: 500), isTrue);
  });

  test('松手 + 还有待提交文字 ⇒ 先等它提交，不抢发', () {
    expect(flush(composingActive: true, sinceLastTextChangeMs: 200), isFalse);
  });

  test('输入法迟迟不提交（超过兜底上限）⇒ 把已有的发出去，不能一直等', () {
    expect(flush(composingActive: true, sinceLastTextChangeMs: kComposingWaitCapMs),
        isTrue);
  });

  test('合并窗口内（<80ms）先不发，等同一批的零碎更新到齐', () {
    expect(flush(sinceLastTextChangeMs: 30), isFalse);
  });

  test('没有文本 ⇒ 不发（按了下麦克风什么都没说）', () {
    expect(flush(textLength: 0), isFalse);
  });

  test('打字（非语音）：拼音还在候选框里 ⇒ 等用户选词，不设兜底', () {
    expect(
        flush(
            voiceSession: false,
            composingActive: true,
            sinceLastTextChangeMs: 5000),
        isFalse);
  });

  test('打字（非语音）：候选框已空 ⇒ 沿用旧的 300ms 等待，避免逐字发送', () {
    expect(flush(voiceSession: false, sinceLastTextChangeMs: 100), isFalse);
    expect(flush(voiceSession: false, sinceLastTextChangeMs: kTypedQuietMs), isTrue);
  });

  test('读不到录音状态时：本判定不启用（调用方走旧行为），这里只验证不抛错', () {
    expect(flush(recAvailable: false, sinceLastTextChangeMs: 500), isTrue);
  });
}
