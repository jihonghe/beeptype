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
