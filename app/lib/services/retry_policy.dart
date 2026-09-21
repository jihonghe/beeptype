/// 断线后的自动重连节奏（纯策略，便于单测）。
///
/// 设计取舍（与用户一起定的）：**快速试一阵，然后停手**。
/// 慢速重试（比如每 30 秒一次）价值很低——真到能连上的时候，用户还不如当场点一下
/// 「重新连接」；反之若不能及时自动连上，长期定时器只是白耗电（在移动数据上还会
/// 反复把无线电拉回高功耗）。
///
/// 因此：约 1 分钟内快速退避重试；试满即停，界面显示「已停止重试」+「重新连接」。
/// 以下事件会**重新开始**一轮快速重试（都表示"用户回来了 / 环境变了"）：
/// 切回前台、网络变化、用户点「重新连接」、重新扫码。
library;

/// 快速重试的间隔序列（毫秒），累计约 65 秒
const List<int> retryDelaysMs = [1000, 2000, 4000, 8000, 10000, 10000, 10000, 10000, 10000];

/// 第 [attempt] 次（从 0 起）重试前的等待时长
int retryDelayMs(int attempt) => attempt < retryDelaysMs.length
    ? retryDelaysMs[attempt]
    : retryDelaysMs.last;

/// 还有没有下一次重试：试满 [retryDelaysMs] 这么多轮就停手
bool retryAllowed(int attempt) => attempt < retryDelaysMs.length;

/// 一轮快速重试的总时长（毫秒），仅用于文档与测试断言
int get retryEpisodeMs => retryDelaysMs.fold(0, (a, b) => a + b);

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

/// 胶囊右侧的按钮
enum PillAction {
  /// 不显示按钮（正在连接/重试中——避免用户重复触发；或还没绑定过电脑）
  none,

  /// 连接（连不上、主动断开、电脑被占用且本机是绑定手机时点它即接管）
  connect,

  /// 断开（已连接）
  disconnect,
}

/// 胶囊里那个词：**只有"在忙"时才有**，其余状态一律不说话。
///
/// 用户需要知道的例外（连不上、被占用）放在打字区——状态归胶囊、该做什么归打字区。
String? pillWord({required bool busy}) => busy ? '连接中' : null;

/// 胶囊右侧显示哪个按钮（纯逻辑，便于单测）。
///
/// - 已连接 → 断开
/// - 正在连接/重试中 → 不显示（否则用户会重复点击）
/// - 其余可操作状态（连不上、主动断开）→ 连接
/// - 还没绑定过电脑 → 不显示（正文就是扫码引导页）
PillAction pillAction({
  required bool connected,
  required bool busy,
  required bool hasTarget,
}) {
  if (connected) return PillAction.disconnect;
  if (busy) return PillAction.none;
  return hasTarget ? PillAction.connect : PillAction.none;
}
