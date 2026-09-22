/// 一轮"快速重试"的状态机（纯逻辑，无 Flutter 依赖，便于彻底单测）。
///
/// ## 它要解决的问题
///
/// 重试这件事此前是一堆散落的布尔与定时器拼出来的，出过两次同类事故：
/// 1. **倒计时归零了重试还在跑**（按"试满 N 次"判断一轮结束，而每次尝试本身
///    要花 1～4 秒，真实时长接近翻倍）；
/// 2. **一轮结束后又冒出一轮**（"新的一轮"的条件是"倒计时定时器不存在"，
///    而到点收掉倒计时正好把它变成不存在 → 任何后续触发都开了一轮新的，
///    包括那次还在飞的尝试收尾时）。
///
/// 所以这里把"一轮"变成一个**显式状态**，并定死两条规则：
/// - 一轮只能由**明确的触发**开启（用户点连接 / 回到前台 / 网络变化 / 冷启动 / 刚断开）；
/// - 一轮内部的尝试失败**只延续当前这一轮**，永远不会自己开新的一轮；
///   到点（墙上时钟）就结束，之后彻底停手，等下一次触发。
library;

class RetryEpisode {
  /// 一轮的时长（墙上时钟）——倒计时与它同一个基准
  final Duration episodeDuration;

  /// 相邻两次尝试的间隔（等间隔）
  final Duration interval;

  /// 兜底上限：一轮最多排多少次重试（时长才是权威，这个只是保险丝）
  final int maxAttempts;

  bool _active = false;
  int _attempt = 0;

  /// 本轮的截止时刻（毫秒）——**开轮那一刻就定下**，所以倒计时立刻可见。
  ///
  /// 曾经想"等第一次重试真的排上再定"，为的是躲开"只有移动数据时倒计时亮一下就灭"；
  /// 但那样会让用户看到一段"显示连接中、却没有倒计时"的空白（用户实测反馈），
  /// 而"亮一下就灭"其实是个诚实的结论——它表示"已经判断出到不了电脑"。
  /// 两个取舍里后者可接受、前者不可接受（用户拍板）。
  int? _deadlineMs;

  RetryEpisode({
    this.episodeDuration = const Duration(seconds: 30),
    this.interval = const Duration(seconds: 2),
    int? maxAttempts,
  }) : maxAttempts = maxAttempts ??
            (episodeDuration.inMilliseconds ~/ interval.inMilliseconds);

  /// 本轮截止时刻（毫秒）；还没开始重试时为 null
  int? get deadlineMs => _deadlineMs;

  /// 已经排过多少次重试
  int get attempt => _attempt;

  /// 开一轮（**只由明确的触发调用**）。已在跑就重置成全新的一轮：
  /// 环境变了 / 用户回来了，给足整整一轮的时间。
  /// 截止时刻当场定下——倒计时与"连接中"同时出现，不留空白。
  void start(int nowMs) {
    _active = true;
    _attempt = 0;
    _deadlineMs = nowMs + episodeDuration.inMilliseconds;
  }

  /// 这一轮现在是否仍在进行。
  ///
  /// **到点即 false**，并顺手把这一轮收干净——**不依赖任何人调用 [shouldSchedule]**。
  /// 出过一次事故：某条路径（"已连接时从扫码页返回"）开了轮却没人来问，
  /// 于是倒计时结束后「连接中」一直赖在界面上（用户实测："已连接 + 连接中 · N 秒
  /// + 断开"同屏）。所以"到点"这件事必须由状态机自己算出来。
  bool isActive(int nowMs) {
    if (!_active) return false;
    if (_deadlineMs != null && nowMs >= _deadlineMs!) {
      _active = false;
      return false;
    }
    return true;
  }

  /// 结束一轮（到点、用户中断、连上、放弃）：之后不再自动重试，等下一次触发。
  void end() {
    _active = false;
    _attempt = 0;
    _deadlineMs = null;
  }

  /// 现在还要不要排下一次重试？false = 这一轮到此为止。
  bool shouldSchedule(int nowMs) {
    if (!isActive(nowMs)) return false;
    if (_attempt >= maxAttempts) {
      _active = false;
      return false;
    }
    return true;
  }

  /// 取下一次重试的等待时长。最后一次等待会被压进剩余时间里，
  /// 避免到点后又多等一次。
  int nextDelayMs(int nowMs) {
    final deadline = _deadlineMs ?? (nowMs + episodeDuration.inMilliseconds);
    final remaining = deadline - nowMs;
    _attempt++;
    final delay = interval.inMilliseconds;
    if (remaining <= 0) return 0;
    return delay < remaining ? delay : remaining;
  }

  /// 倒计时显示用的剩余秒数（向上取整）；**没在跑（含已到点）返回 null**，
  /// 界面据此不显示倒计时、也不再说「连接中」。
  int? remainingSeconds(int nowMs) {
    if (!isActive(nowMs)) return null;
    return (_deadlineMs! - nowMs + 999) ~/ 1000;
  }
}
