import 'package:flutter_test/flutter_test.dart';

import 'package:beeptype/services/voice_send_policy.dart';

/// "松手才发"的护栏：录音中一个字都不发；松手 + 安静够久才把整段发出去。
///
/// 真机依据（2026-09-23 诊断）：按住语音键期间麦克风一直被占用（连停顿 5 秒也不放），
/// 松手立刻释放；分段型输入法中途的每次提交都发生在录音期间。
void main() {
  bool flush({
    bool recAvailable = true,
    bool recording = false,
    int textLength = 10,
    int sinceLastTextChangeMs = 1000,
    int quietMs = kRecordingEndQuietMs,
  }) =>
      shouldFlushOnRecordingEnd(
        recAvailable: recAvailable,
        recording: recording,
        textLength: textLength,
        sinceLastTextChangeMs: sinceLastTextChangeMs,
        quietMs: quietMs,
      );

  test('录音中：中途提交多少次都不发（这是碎片问题的正解）', () {
    expect(flush(recording: true, sinceLastTextChangeMs: 5000), isFalse);
  });

  test('松手 + 安静够久 ⇒ 发', () {
    expect(flush(recording: false, sinceLastTextChangeMs: kRecordingEndQuietMs),
        isTrue);
    expect(flush(recording: false, sinceLastTextChangeMs: 3000), isTrue);
  });

  test('松手但还没安静够（输入法最后一段尚未提交）⇒ 先不发', () {
    expect(flush(recording: false, sinceLastTextChangeMs: 200), isFalse);
  });

  test('没有文本 ⇒ 不发（按了下麦克风什么都没说）', () {
    expect(flush(textLength: 0), isFalse);
  });

  test('读不到录音状态时：没有"录音中"这个约束，只按安静窗口（调用方走旧行为）', () {
    expect(flush(recAvailable: false, sinceLastTextChangeMs: 3000), isTrue);
    expect(flush(recAvailable: false, sinceLastTextChangeMs: 100), isFalse);
  });

  test('不用语音、直接在手机上打字：recording 恒为 false，安静后照旧发', () {
    expect(flush(recAvailable: true, recording: false, sinceLastTextChangeMs: 900),
        isTrue);
  });
}
