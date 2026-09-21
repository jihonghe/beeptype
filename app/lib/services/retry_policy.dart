/// 断线后的**文案与动作选择**（纯逻辑，便于单测）。
///
/// "一轮快速重试"的节奏与状态机在 retry_episode.dart；这里放的是"说不说、说什么、
/// 给哪个按钮"这类判断。
///
/// 一轮的节奏（等间隔 2 秒、共 30 秒、到点停手）由 retry_episode.dart 的状态机负责。
/// 这里只回答"界面说什么"：胶囊里的词（连接中 · N 秒 / 无）、给哪个按钮（连接 /
/// 中断 / 断开 / 不给）、以及"电脑拒绝时该不该当成本机被解绑"。
library;

/// 这次"对端关闭连接"该不该算在**电脑侧**（临时接入据此决定要不要丢记录）。
///
/// 只有同时满足两条才算电脑侧：
/// - [weWereTalking]：关闭发生时我们自己还在正常通信（不是被系统挂起、静默到被 PC 收尸）；
/// - [netJustChanged] == false：手机自己的网络没有刚刚变化过（不是我们自己换网络导致的连接结束）。
///
/// 任一不满足，锅就在手机自己这边——临时记录要保留，给它在窗口期内续连的机会。
bool peerCloseIsReceiverSide({
  required bool peerClosed,
  required bool weWereTalking,
  required bool netJustChanged,
}) =>
    peerClosed && weWereTalking && !netJustChanged;

/// 这次"凭证被拒"是不是**我们那台电脑**给的？
///
/// 手机拿存储的地址去连时，那个地址可能已经被**另一台设备**占用（含另一台跑着
/// BeeType 的电脑）。它会拒绝我们的凭证，但那并不代表"我们被解绑了"——只有
/// 记忆里那台电脑的拒绝才算数。拿不到对方 device_id（旧版电脑）时按"是"处理，
/// 保持旧行为。
bool rejectionFromOurPc({required String? responderId, required String? expectedId}) {
  if (responderId == null || responderId.isEmpty) return true;
  if (expectedId == null || expectedId.isEmpty) return true;
  return responderId == expectedId;
}

/// 电脑拒绝本机凭证时的那句话（纯逻辑，便于单测）。
///
/// 两种情况必须分开说：
/// - **绑定手机**被拒 = 真被解绑了 → 「电脑已解除与你的绑定，请重新扫码」；
/// - **临时接入**被拒 = 电脑把这次临时连接忘了（PC 服务重启/退出，接收方自身
///   出问题就不给机会）或窗口真过期 → 「连接已失效，请重新扫码」，
///   不能谎称"被解除绑定"（那会让用户以为是自己被踢了）。
String credentialRejectedMessage({required bool bound}) =>
    bound ? '电脑已解除与你的绑定，请重新扫码' : '连接已失效，请重新扫码';

/// 胶囊里那个词：**只有"在忙"时才有**，其余状态一律不说话。
///
/// 用户需要知道的例外（连不上、被占用）放在打字区——状态归胶囊、该做什么归打字区。
String? pillWord({required bool busy, int? secondsLeft}) {
  if (!busy) return null;
  // 0 秒不显示：那一刻通常还有一次尝试在飞，写「· 0 秒」只会让人以为卡住了
  if (secondsLeft == null || secondsLeft <= 0) return '连接中';
  return '连接中 · $secondsLeft 秒';
}

/// 胶囊右侧的按钮
enum PillAction {
  /// 不显示按钮（还没绑定过电脑；正文就是扫码引导页）
  none,

  /// 中断：重试中给的动作——立刻停手，不是"再连一次"（避免重复触发）
  stop,

  /// 连接（连不上、主动断开、电脑被占用且本机是绑定手机时点它即接管）
  connect,

  /// 断开（已连接）
  disconnect,
}

/// 胶囊右侧显示哪个按钮（纯逻辑，便于单测）。
///
/// - 已连接 → 断开
/// - 正在连接/重试中 → **中断**（让用户随时能打断，而不是给一个"再连一次"的按钮）
/// - 其余可操作状态（连不上、主动断开）→ 连接
/// - 还没绑定过电脑 → 不显示（正文就是扫码引导页）
PillAction pillAction({
  required bool connected,
  required bool busy,
  required bool hasTarget,
}) {
  if (connected) return PillAction.disconnect;
  if (busy) return PillAction.stop;
  return hasTarget ? PillAction.connect : PillAction.none;
}
