/// "松手才发"的判定（纯函数，便于单测）。
///
/// 背景：分段型输入法（讯飞/搜狗）会在说话途中逐段提交文本——**手指没离开语音键也会提交**。
/// 我们原来把"composing 结束"当成"用户松手"，于是把一整段话打成碎片逐段发出去。
///
/// 2026-09-23 真机诊断确认：**麦克风占用状态**能可靠区分两者——按住语音键期间麦克风一直被
/// 占用（连停顿 5 秒也不放），松手那一刻立刻释放。故：**还在录音就一个字都不发**。
///
/// ## 松手之后等什么（延迟优化的关键）
///
/// 早先的写法是"松手后固定等 800ms"，那是盲等——它唯一要防的情况是"输入法在你松手**之后**
/// 才提交最后一段"（实测最晚约 0.6 秒）。而这个情况**可以直接判断**：松手那一刻输入框里
/// 还有没有"没提交的文字"（composing 还开着）。
///
/// - **没有待提交的文字** ⇒ 立刻发，只留一个很小的合并窗口（语音 80ms），
///   用来把同一批的零碎更新并成一次。→ 微信/豆包这类"边说边提交"的输入法走这条，松手即出。
/// - **还有待提交的文字** ⇒ 等它提交（等到了就发）。这段等待不可省，否则会把完整的一段
///   拆成两条发出去。→ 讯飞/搜狗走这条，松手后 ≈ 输入法自己的提交延迟（实测约 0.5 秒）。
///   兜底：语音场景下超过 [kComposingWaitCapMs] 还没提交（输入法卡住）⇒ 直接把已有的发出去，
///   宁可少最后半句，也不能让用户觉得"说了话什么都没发生"。
/// - **打字场景**（拼音还在候选框里）不设兜底，等用户选完词——与旧行为一致。
///
/// 读不到麦克风状态时（老系统把"谁在录音"匿名化、iOS 未实现）整条机制不启用，
/// 由调用方走旧行为（composing 结束即发），本函数不参与。
bool shouldFlush({
  required bool recAvailable,
  required bool recording,
  required int textLength,
  required bool composingActive,
  required bool voiceSession,
  required int sinceLastTextChangeMs,
}) {
  if (textLength == 0) return false;
  if (recAvailable && recording) return false; // 还在录音：一个字都不发

  if (composingActive) {
    // 还有没提交的文字：等输入法提交；语音场景加兜底，打字场景等用户选词
    return voiceSession && sinceLastTextChangeMs >= kComposingWaitCapMs;
  }

  // 没有待提交的文字：只等合并窗口
  return sinceLastTextChangeMs >= (voiceSession ? kVoiceSettleMs : kTypedQuietMs);
}

/// 语音：松手后没有待提交文字时的合并窗口（把同一批的零碎更新并成一次）
const int kVoiceSettleMs = 80;

/// 打字：手机键盘输入后的等待（与旧行为一致，避免逐字发送）
const int kTypedQuietMs = 300;

/// 语音兜底：还有待提交文字、但输入法迟迟不提交时的上限
const int kComposingWaitCapMs = 1200;

/// 观察麦克风占用状态的间隔：越小越灵敏，150ms 是"够快且不费电"的折中
const int kRecPollIntervalMs = 150;
