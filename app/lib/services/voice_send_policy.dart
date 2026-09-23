/// "松手才发"的判定（纯函数，便于单测）。
///
/// 背景：分段型输入法（讯飞/搜狗）会在说话途中逐段提交文本——**手指没离开语音键也会提交**。
/// 我们原来把"composing 结束"当成"用户松手"，于是把一整段话打成碎片逐段发出去。
///
/// 2026-09-23 真机诊断（三款输入法各说一段，日志 `BT_IME`）确认：
/// **麦克风占用状态**能可靠区分两者——按住语音键期间麦克风一直被占用（连停顿 5 秒也不放），
/// 松手那一刻立刻释放；而输入法的最后一段文本要再晚 0.5~1 秒才提交。
///
/// 规则：**还在录音就一个字都不发**；不录音 + 有文本 + 距最后一次文本变化超过静默窗口 ⇒ 发。
///
/// 读不到麦克风状态时（老系统把"谁在录音"匿名化、iOS 未实现）**退回旧行为**
/// （composing 结束即发），由调用方判断，本函数不处理。
bool shouldFlushOnRecordingEnd({
  required bool recAvailable,
  required bool recording,
  required int textLength,
  required int sinceLastTextChangeMs,
  required int quietMs,
}) {
  if (textLength == 0) return false;
  if (sinceLastTextChangeMs < quietMs) return false;
  if (recAvailable && recording) return false;
  return true;
}

/// 松手后的静默窗口：实测"松手 → 输入法提交最后一段"约 0.5~1 秒，
/// 取 800ms 再叠加一次轮询间隔（500ms）即可覆盖，同时不至于让用户觉得"松手了没反应"。
const int kRecordingEndQuietMs = 800;
