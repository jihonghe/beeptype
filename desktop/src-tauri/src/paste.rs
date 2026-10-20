//! 自动粘贴：写好剪贴板后模拟系统快捷键，把文字送进当前焦点窗口的光标处。
//!
//! 平台策略：
//! - Linux X11：enigo 走 XTest 扩展（x11rb，纯 Rust），无需任何授权；
//!   并通过焦点窗口的 WM_CLASS 识别终端，终端发 Ctrl+Shift+V，其余发 Ctrl+V；
//! - Linux Wayland：能不能注入取决于**合成器**，不取决于 Wayland 本身。
//!   wlroots 系（Hyprland、Sway）暴露 `zwp_virtual_keyboard_manager_v1`
//!   （`wayland_input.rs` 探测），enigo 的 wayland 后端就走它 ⇒ 自动粘贴可用；
//!   GNOME/KDE 不暴露该协议 ⇒ 判定不支持，回退纯剪贴板 + 手动 Ctrl+V。
//!   注意 Wayland 下**拿不到焦点窗口**，所以终端识别（Ctrl+Shift+V）用不了，
//!   一律发 Ctrl+V（终端里的用户需要自己按终端的粘贴键）；
//! - Windows：SendInput，无需授权（UIPI 隔离下无法输入到更高权限窗口），
//!   统一发 Ctrl+V（现代 Windows Terminal 也接受 Ctrl+V）；
//! - macOS：CGEvent，Cmd+V 全局一致（终端同样是 Cmd+V）。
//!
//! 时序策略（细节见 docs/TECH_DESIGN.md 第 10 节）：
//! - 粘贴快捷键与回车复用同一个 enigo 实例，同一条连接内由 compositor/X server
//!   保证按序处理，不依赖墙钟时间排序；
//! - 注入回车前复核焦点窗口没变，变了就不发——避免把回车送给用户刚切过去的窗口
//!   （Wayland 下拿不到焦点，这项复核自动退化为"照常发送"）；
//! - 粘贴与回车之间的等待是**安全余量**而不是性能参数：目标应用（尤其
//!   Chromium/Electron 系）读剪贴板是异步的，回车抢先执行就会变成空发送，
//!   而人对这段延迟无感知，所以取值刻意宽松，且不做按机器/按应用的特调。

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use log::{debug, info, warn};
use std::time::{Duration, Instant};

/// 写剪贴板后、发送快捷键前的等待。
///
/// 正确性并不依赖它：写入在返回前已同步声明所有权（见 clipboard.rs）。
/// 这里的 50ms 是给"目标应用剪贴板缓存 + 归属权切换"的额外余量。
const PRE_PASTE_DELAY: Duration = Duration::from_millis(50);

/// 向上遍历窗口树的最大层数（焦点常落在终端内部的 VTE 子窗口上）
const MAX_WINDOW_TRAVERSAL: u32 = 20;

/// 粘贴之后、发回车之前的基准余量。
///
/// 必须显式等待：目标应用可能是异步插入（Chromium 系由浏览器进程读剪贴板、
/// 再经 IPC 交给渲染进程插入），回车若抢先执行就是一句空发送。这个值取
/// "人感知不到的最宽松值"——200~300ms 用户完全无感，而漏发的代价是消息没出去。
const ENTER_BASE_DELAY: Duration = Duration::from_millis(250);

/// 每多少个字符追加 1ms 余量：内容越长，目标插入越慢。这是内容维度的通用
/// 规律（与机器无关），因此可以放心写成公式，而不是按机型标定。
const ENTER_EXTRA_MS_PER_CHARS: usize = 200;

/// 图片粘贴后、回 ack 之前的基准余量。
///
/// 为什么图片需要它：**手机是一张等一个 ack 才发下一张**，而剪贴板只有一个槽位。
/// 我们注入 Ctrl+V 只代表"消息发出去了"，目标应用（Chromium/Electron 系）读剪贴板
/// 是异步的——若这时立刻回 ack，下一张的写入会赶在目标读完这一张之前把它覆盖掉，
/// 结果就是"选 3 张只到 2 张"（2026-10-18 复现：三张图三次粘贴读到的全是最后一张）。
/// 所以余量必须补在 ack 之前：它是"下一张什么时候能来"的唯一闸门。
const IMAGE_SETTLE_BASE: Duration = Duration::from_millis(300);

/// 每 100KB 追加 10ms：图越大，目标读得越慢（与文本的按字数递增同一条规律）
const IMAGE_SETTLE_EXTRA_MS_PER_100KB: usize = 10;
/// 100KB 的字节数
const IMAGE_SETTLE_EXTRA_STEP_BYTES: usize = 100 * 1024;

/// 追加余量的上限，避免超大图把 ack 拖得过久
const IMAGE_SETTLE_MAX_EXTRA: Duration = Duration::from_millis(700);

/// 图片粘贴余量：基准 + 按体积有界递增（上限合计 1 秒）
pub fn image_settle_delay(image_bytes: usize) -> Duration {
    let extra_ms = (image_bytes / IMAGE_SETTLE_EXTRA_STEP_BYTES) * IMAGE_SETTLE_EXTRA_MS_PER_100KB;
    let extra_ms = extra_ms.min(IMAGE_SETTLE_MAX_EXTRA.as_millis() as usize);
    IMAGE_SETTLE_BASE + Duration::from_millis(extra_ms as u64)
}

/// 等满图片余量（调用方在注入粘贴键之后、回 ack 之前调用，见 `image_settle_delay`）
pub fn wait_image_settle(image_bytes: usize) {
    std::thread::sleep(image_settle_delay(image_bytes));
}

/// 目标应用"消化"一张刚粘贴的图片所需的时间（从粘贴那一刻算起）。
///
/// 为什么需要它：图片粘进输入框后，应用还要异步把这份图读走、再上传/入库。
/// 在这个窗口里按回车，应用**不当作"发送"**，而是当普通换行 ⇒ 表现为"图文消息
/// 变成换行、没发出去"，且随图片大小忽有忽无（看着像"不稳定复现"）。
///
/// **量纲是像素，不是手机发来的字节数**：剪贴板里必须是位图（arboard 编成 PNG），
/// 而 Chromium/Electron 只接受 `image/png`（2026-10-21 实测 jpeg/webp/bmp/tiff 全被
/// 忽略）⇒ 应用吞的是"像素数换算出来的 PNG"，与源 JPEG 的压缩率无关。
///
/// **必须按"端到端"算，不能只按上传那一段**（2026-10-21 踩过：第一版按上传速率
/// 8MB/s 取了 300ms/百万像素，结果真机上又变回换行）。应用是**先把剪贴板数据读走、
/// 再上传/入库**，两段加起来才是它不能收回车的时间。实测三次（ZCode）：
/// 剪贴板 1.5MB→0.32s、5.49MB→1.32s、9.5MB→2.25s ⇒ **端到端 ≈4.2MB/s**。
/// 照片类 PNG ≈2 字节/像素 ⇒ ≈0.48µs/像素；这里取 0.7µs/像素（+45% 余量），
/// 因为估小了就会重演"换行、没发出去"。
///
/// 为什么是时间而不是"等应用就绪"：没有可感知的信号。剪贴板不提供"数据被读走"
/// 事件（见 docs/TECH_DESIGN.md 10.2），应用的内部状态对外不可见；读无障碍树
/// 或盯进程 I/O 都要按应用/按平台猜，成本和脆弱性远超一个按像素算的余量。
///
/// 2026-10-21 起剪贴板侧还把长边压到 1920（`image_receiver::CLIPBOARD_MAX_EDGE`），
/// 所以这个预算实际是 1~2.5s 量级；上限仍保留 4s，兜住更慢的应用。
const IMAGE_DIGEST_BASE: Duration = Duration::from_millis(100);
const IMAGE_DIGEST_PER_MP: Duration = Duration::from_millis(700);
const IMAGE_DIGEST_MAX_WAIT: Duration = Duration::from_secs(4);

/// 追加余量的上限，避免超长文本把回车拖得过久
const ENTER_EXTRA_MAX_DELAY: Duration = Duration::from_millis(500);

/// 回车前的等待时长：基准余量 + 按文本长度有界递增
fn enter_delay(text_chars: usize) -> Duration {
    let cap_ms = ENTER_EXTRA_MAX_DELAY.as_millis() as usize;
    let extra_ms = (text_chars / ENTER_EXTRA_MS_PER_CHARS).min(cap_ms);
    ENTER_BASE_DELAY + Duration::from_millis(extra_ms as u64)
}

/// 单张图片的消化时长：基准余量 + 按**写进剪贴板的像素数**递增
/// （不设上限，上限在等待端统一收口）
pub fn image_digest_delay(pixels: u64) -> Duration {
    let per_mp = (pixels as u128 * IMAGE_DIGEST_PER_MP.as_millis()) / 1_000_000;
    IMAGE_DIGEST_BASE + Duration::from_millis(per_mp as u64)
}

/// 把"又粘了一张图"接到消化队列上，返回队列预计清空的时刻。
///
/// 模型是**串行**的：应用同时消化多张图时按顺序做，所以每张图的起点取
/// "它被粘贴的时刻"与"上一张消化完的时刻"里较晚的那个。手机是一张等一个 ack
/// 才发下一张，真实场景里两者相差不大；这里宁可高估（多等）也不低估（漏发）。
///
/// `ready_at` 为 `None`（还没贴过图，或队列早已清空）时就是"从这一刻开始算一张"。
pub fn queue_image_digest(
    ready_at: Option<Instant>,
    pasted_at: Instant,
    pixels: u64,
) -> Instant {
    let start = match ready_at {
        Some(ready) if ready > pasted_at => ready,
        _ => pasted_at,
    };
    start + image_digest_delay(pixels)
}

/// 距离"图片消化完"还要等多久：已经消化完（或压根没贴过图）返回 `None`。
///
/// **这就是"纯文字消息不受影响"的保证点**：没贴过图 ⇒ `ready_at` 是 `None`；
/// 贴过但已经等过了 ⇒ 目标时刻在过去 ⇒ 同样返回 `None`。两种情况下调用方都不睡。
pub fn remaining_digest_wait(ready_at: Option<Instant>, now: Instant) -> Option<Duration> {
    let left = ready_at?.saturating_duration_since(now);
    if left.is_zero() {
        None
    } else {
        Some(left.min(IMAGE_DIGEST_MAX_WAIT))
    }
}

/// 精确命中这些值时视为终端（名字不含 "terminal" 的异类）。
///
/// 两个平台都用这一份：X11 拿 WM_CLASS（`xterm`、`kitty`…），Wayland 拿 app_id
/// （`foot`、`kitty`，也可能是反向域名形式 `com.mitchellh.ghostty`、
/// `org.wezfurlong.wezterm`）。判定顺序见 `is_terminal_name`。
const TERMINAL_CLASSES: &[&str] = &[
    "xterm",
    "uxterm",
    "rxvt",
    "urxvt",
    "rxvt-unicode",
    "st",
    "kitty",
    "alacritty",
    "ghostty",
    "wezterm",
    "termite",
    "konsole",
    "yakuake",
    "guake",
    "tilda",
    "sakura",
    "foot",
    "foot-client",
    "tabby",
    "hyper",
    "putty",
    "eterm",
    "e-term",
    "warp",
    "termius",
    "ptyxis",
    "blackbox",
    "cool-retro-term",
];

/// 本会话实际可用的按键注入后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionBackend {
    /// 无法模拟全局输入（GNOME/KDE 的 Wayland 等）⇒ 回退纯剪贴板 + 手动粘贴
    Unsupported,
    /// 平台原生注入：Linux X11 的 XTest / Windows 的 SendInput / macOS 的 CGEvent
    Native,
    /// wlroots 系 Wayland（Hyprland、Sway）：走合成器的虚拟键盘协议
    WlrWayland,
}

impl InjectionBackend {
    /// 用于启动日志，方便一眼看出这台机器走的哪条路
    pub fn label(self) -> &'static str {
        match self {
            InjectionBackend::Unsupported => "unsupported",
            InjectionBackend::Native => "native",
            InjectionBackend::WlrWayland => "wayland-wlr",
        }
    }
}

/// 当前会话是不是 Wayland。
///
/// 判据是"有没有 Wayland 显示"而不是"XDG_SESSION_TYPE 写没写"：X11 会话里
/// 这个变量可能缺省，而反过来只要 `WAYLAND_DISPLAY` 在，目标应用就有可能是
/// 原生 Wayland 客户端——那 XTest 就注不进去。
fn session_is_wayland() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|t| t == "wayland")
                .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// 会话能力判定（探测结果抽成纯函数，便于单测覆盖各种组合）
fn decide_backend(
    wayland_session: bool,
    wayland_virtual_input: bool,
    native_display: bool,
) -> InjectionBackend {
    if wayland_session {
        // Wayland 下 XTest 到不了原生 Wayland 客户端，只能靠合成器的虚拟输入协议
        if wayland_virtual_input {
            InjectionBackend::WlrWayland
        } else {
            InjectionBackend::Unsupported
        }
    } else if native_display {
        InjectionBackend::Native
    } else {
        InjectionBackend::Unsupported
    }
}

/// 当前会话可用的注入后端（Wayland 下含一次合成器能力探测）。
///
/// 结果按进程缓存：会话能力（Wayland 有没有虚拟输入协议、DISPLAY 在不在）
/// 运行期不会变——合成器/会话换了，本进程也该重启了。缓存前 Wayland 下
/// **每次调用都要新建一条合成器连接、把全局接口列表读一遍**，而取一次设置
/// 就会经 auto_paste/auto_send/terminal_paste_ready 连调三次。
pub fn injection_backend() -> InjectionBackend {
    static CACHED: std::sync::OnceLock<InjectionBackend> = std::sync::OnceLock::new();
    *CACHED.get_or_init(probe_injection_backend)
}

fn probe_injection_backend() -> InjectionBackend {
    let wayland_session = session_is_wayland();
    let wayland_virtual_input = wayland_session && crate::wayland_input::virtual_input_available();

    #[cfg(target_os = "linux")]
    let native_display = std::env::var_os("DISPLAY").is_some();
    // Windows/macOS 不需要"显示"这个概念，走到 Native 分支即可
    #[cfg(not(target_os = "linux"))]
    let native_display = true;

    decide_backend(wayland_session, wayland_virtual_input, native_display)
}

/// 当前会话是否支持全局按键模拟（自动粘贴）。
pub fn auto_paste_supported() -> bool {
    injection_backend() != InjectionBackend::Unsupported
}

/// Wayland 会话下传给 enigo 的"连不上的 display 名"。
///
/// enigo 会把每个按键**发给所有已连接的后端**，而 Hyprland 这类会话里
/// `DISPLAY`（XWayland）与 `WAYLAND_DISPLAY` 同时存在 ⇒ 两个后端都连上 ⇒
/// 一次 Ctrl+V 被注入两遍（实测：XWayland 目标里文本被粘贴两次）。
/// 传一个连不上的 display 名就能让它放弃 X11 后端、只留虚拟键盘那条路
/// （连接失败不影响注入；enigo 会为此打一行 error 级日志，已在 lib.rs 里
/// 把 `enigo` 整条日志关掉）。
#[cfg(target_os = "linux")]
const X11_DISABLED_DISPLAY: &str = "beeptype-wayland-only";

/// 这次粘贴用哪条路（抽成纯函数，把"哪个平台哪个目标用哪种注入"钉进单测）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastePlan {
    /// 用 enigo 发快捷键（`with_shift` = 终端里的 Ctrl+Shift+V）
    Enigo { with_shift: bool },
    /// Wayland 终端：交给外部 `wtype` 发 Ctrl+Shift+V（见 `wtype_paste_in_terminal`）
    WtypeTerminal,
}

fn paste_plan(backend: InjectionBackend, terminal: bool) -> PastePlan {
    if backend == InjectionBackend::WlrWayland && terminal {
        PastePlan::WtypeTerminal
    } else {
        PastePlan::Enigo { with_shift: terminal }
    }
}

/// 终端里的自动粘贴是否可用：Wayland（wlroots）下需要外部 `wtype`（见下）。
///
/// 只影响"终端窗口"这一格——没装 `wtype` 时终端退化成手动粘贴（文字/图片仍在
/// 剪贴板里，用户自己按 Ctrl+Shift+V），其它窗口的自动粘贴不受影响。
/// 设置页据此给用户一行提示（不做弹窗/通知：它是能力说明，不是错误）。
pub fn terminal_paste_ready() -> bool {
    terminal_paste_ready_for(injection_backend(), wtype_in_path())
}

/// 判定抽成纯函数，便于单测覆盖（含非 Linux 分支）
fn terminal_paste_ready_for(backend: InjectionBackend, wtype_present: bool) -> bool {
    backend != InjectionBackend::WlrWayland || wtype_present
}

/// PATH 里有没有可执行的 `wtype`。自己走 PATH 而不是起进程/调 `which`：
/// 只读文件元数据，零副作用。
///
/// 结果缓存：它只用于设置页那一行能力提示，而运行期装上 wtype 的情况很少；
/// 真装了重启应用即可（缓存前每次取设置都要把 PATH 扫一遍）。
#[cfg(target_os = "linux")]
fn wtype_in_path() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(scan_wtype_in_path)
}

#[cfg(target_os = "linux")]
fn scan_wtype_in_path() -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        match std::fs::metadata(dir.join("wtype")) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn wtype_in_path() -> bool {
    // 非 Linux 不走这条路（Windows/macOS 的终端粘贴用各自的注入）
    true
}

/// Wayland 会话下往终端粘贴：用 wtype 发 Ctrl+Shift+V。
///
/// 为什么不用 enigo：它给修饰键只发协议里那个"修饰位"请求（`modifiers`）、不发按键
/// 事件。实测在 Hyprland 上 Shift 因此到不了应用（靶子只看到 `GDK_CONTROL_MASK`），
/// 终端于是不粘贴；而 wtype 把修饰键当普通按键按下，实测能正确送达
/// （靶子收到 `mods=['GDK_SHIFT_MASK','GDK_CONTROL_MASK']`，foot 里粘贴成功）。
///
/// wtype 不在 PATH 或执行失败时返回 Err：调用方按"没粘上"处理，文字仍在剪贴板里，
/// 用户自己按终端的粘贴键即可（不静默假装成功）。
#[cfg(target_os = "linux")]
fn wtype_paste_in_terminal() -> Result<(), String> {
    let output = std::process::Command::new("wtype")
        .args([
            "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
        ])
        .output()
        .map_err(|e| format!("wtype 未能启动（没装？）：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "wtype 退出码 {:?}：{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// 构造 enigo 设置：Wayland 会话下只保留虚拟键盘后端（见上）
fn enigo_settings() -> Settings {
    #[allow(unused_mut)]
    let mut settings = Settings::default();

    #[cfg(target_os = "linux")]
    if injection_backend() == InjectionBackend::WlrWayland {
        settings.x11_display = Some(X11_DISABLED_DISPLAY.to_string());
    }

    settings
}

/// 发回车之前的等待（安全余量，见模块文档）。
///
/// **等待与复核的顺序是有意的**：调用方要先 `wait_before_enter()`、再复核焦点、
/// 最后才 `send_enter()`。这样复核覆盖的正是"粘贴完成 → 回车落下"这段几百毫秒——
/// 用户最可能在这段时间里切窗口，而早期实现在粘贴后立刻复核，等于只盯了粘贴本身
/// 那几十毫秒，闸门形同虚设（2026-10-18 在 Wayland 上真机才发现）。
pub fn wait_before_enter(text_chars: usize) {
    std::thread::sleep(enter_delay(text_chars));
}

/// 模拟 Enter 键（调用方需先 `wait_before_enter()` 并完成焦点复核）。
///
/// 复用 simulate_paste() 交回的 enigo 实例，让粘贴与回车走同一条连接：
/// 同连接内的请求由 compositor/X server 按序处理，回车不会乱序跑到粘贴前面。
pub fn send_enter(enigo: &mut Enigo) -> Result<(), String> {
    enigo
        .key(Key::Unicode('\r'), Direction::Click)
        .map_err(|e| format!("enter key: {e}"))
}

/// 自动发送的可用条件和自动粘贴完全一致（同依赖 enigo + 平台输入注入）。
pub fn auto_send_supported() -> bool {
    auto_paste_supported()
}

/// 向当前焦点窗口发送"粘贴"快捷键：
/// - macOS：Cmd+V；
/// - Linux 终端窗口：Ctrl+Shift+V（Wayland 下交给 wtype，见 `wtype_paste_in_terminal`）；
/// - 其余：Ctrl+V。
///
/// 必须在剪贴板写入成功之后调用；调用线程阻塞约 50ms。
/// 任何窗口检测失败都安全回退到 Ctrl+V。
///
/// 成功时交回 enigo 实例，供随后的自动发送**复用同一条连接**；返回 `None` 表示这次
/// 不是用 enigo 发的（Wayland 终端走 wtype），调用方没有可复用的连接。
pub fn simulate_paste() -> Result<Option<Enigo>, String> {
    std::thread::sleep(PRE_PASTE_DELAY);

    #[cfg(target_os = "linux")]
    let terminal = terminal_at_focus();
    #[cfg(not(target_os = "linux"))]
    let terminal = false;

    #[cfg(target_os = "linux")]
    if injection_backend() == InjectionBackend::WlrWayland {
        debug!("Focus at paste time: {:?} (terminal={terminal})", focused_window_key());
    }

    match paste_plan(injection_backend(), terminal) {
        PastePlan::WtypeTerminal => {
            info!("Auto-paste shortcut: Ctrl+Shift+V (wtype)");
            #[cfg(target_os = "linux")]
            wtype_paste_in_terminal()?;
            Ok(None)
        }
        PastePlan::Enigo { with_shift } => {
            let mut enigo =
                Enigo::new(&enigo_settings()).map_err(|e| format!("Enigo init: {e}"))?;
            #[cfg(target_os = "macos")]
            {
                info!("Auto-paste shortcut: Cmd+V");
                send_paste(&mut enigo, Key::Meta, false)?;
            }
            #[cfg(not(target_os = "macos"))]
            {
                info!("Auto-paste shortcut: {}", if with_shift { "Ctrl+Shift+V" } else { "Ctrl+V" });
                send_paste(&mut enigo, Key::Control, with_shift)?;
            }
            Ok(Some(enigo))
        }
    }
}

/// 当前焦点窗口的"标识"，用于注入回车前复核"用户没有切走窗口"。
///
/// - Linux/X11：焦点客户端窗口的 id（`x11:<id>`）；
/// - wlroots 系 Wayland：合成器报的焦点顶层窗口 `wayland:<app_id>|<title>`
///   （`wayland_input.rs` 的跟踪器）。带上标题是有意的：同一个应用开两个窗口时，
///   只比 app_id 会漏判"切到了同一应用的另一个窗口"，而漏判的方向是**多发**一个
///   回车——那正是我们要防的。反过来，标题变化（如终端标题跟着命令变）只会让
///   我们**少发**一次回车，是安全方向；
/// - GNOME/KDE 的 Wayland、非 Linux、焦点读不到：`None`。
///
/// `None` 一律按"无从判断"处理，即保持原行为照常发送（fail-open：宁可漏掉一次
/// 检查，也不因为检查不了而少发一条消息）。
pub fn focused_window_key() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if injection_backend() == InjectionBackend::WlrWayland {
            return crate::wayland_input::current_focus()
                .map(|w| format!("wayland:{}|{}", w.app_id, w.title));
        }
        focus_window_id_and_classes()
            .ok()
            .flatten()
            .map(|(id, _)| format!("x11:{id}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 注入回车前是否应当放弃发送：前后两次焦点标识都读到了、且不一样。
///
/// 两边任一读不到就是"无从判断"，按既有约定照常发送（fail-open）。
/// 抽成纯函数是为了把这条判断钉进单测——它是自动发送唯一的安全闸门。
pub fn should_skip_enter(before: Option<String>, after: Option<String>) -> bool {
    matches!((before, after), (Some(a), Some(b)) if a != b)
}

/// 发送 修饰键(+Shift) 按住 → V 单击 → 全部松开。
fn send_paste(enigo: &mut Enigo, modifier: Key, with_shift: bool) -> Result<(), String> {
    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| format!("modifier press: {e}"))?;
    if with_shift {
        if let Err(e) = enigo.key(Key::Shift, Direction::Press) {
            warn!("Auto-paste shift press failed: {e}");
        }
    }
    let click = enigo.key(Key::Unicode('v'), Direction::Click);
    if with_shift {
        if let Err(e) = enigo.key(Key::Shift, Direction::Release) {
            warn!("Auto-paste shift release failed: {e}");
        }
    }
    if let Err(e) = enigo.key(modifier, Direction::Release) {
        warn!("Auto-paste modifier release failed: {e}");
    }
    click.map_err(|e| format!("paste key: {e}"))
}

/// 当前焦点窗口是不是终端（供自动发送决定要不要按回车）。
///
/// 与粘贴用的是同一套判定，只是**在按回车之前重新读一次**：这几十/几百毫秒里
/// 用户如果把焦点移到了终端，那个回车就不该发（终端里 Enter 是"执行命令"）。
pub fn focus_is_terminal() -> bool {
    #[cfg(target_os = "linux")]
    {
        terminal_at_focus()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// 当前焦点窗口是不是终端。
///
/// - Linux/X11：读焦点窗口（或其祖先）的 WM_CLASS；
/// - wlroots 系 Wayland：读跟踪器给的焦点顶层窗口 app_id；
/// - 读不到、或平台不支持：一律 false（发默认的 Ctrl+V，不猜）。
#[cfg(target_os = "linux")]
fn terminal_at_focus() -> bool {
    if injection_backend() == InjectionBackend::WlrWayland {
        return match crate::wayland_input::current_focus() {
            Some(window) => is_terminal_name(&window.app_id),
            None => {
                debug!("Focus tracker has no focus window yet; assuming not a terminal");
                false
            }
        };
    }

    match focus_window_id_and_classes() {
        Ok(Some((_, classes))) => classes.iter().any(|c| is_terminal_name(c)),
        Ok(None) => false,
        Err(e) => {
            warn!("Focus window detection failed, falling back to Ctrl+V: {e}");
            false
        }
    }
}

/// 名字（X11 的 WM_CLASS 或 Wayland 的 app_id）是不是终端。
///
/// 三条判据，从宽到窄：
/// 1. 含 "terminal" 一律算（覆盖 deepin-terminal / gnome-terminal / xfce4-terminal
///    等一大批），含 "retro-term" 也算；
/// 2. 整个名字精确命中清单（`xterm`、`foot`、`kitty`…）；
/// 3. **取反向域名 app_id 的最后一段**再比对清单——Wayland 客户端习惯用
///    `com.mitchellh.ghostty`、`org.wezfurlong.wezterm`、`app.devsuite.Ptyxis`
///    这种名字，取末段就退化回我们熟悉的那些终端名。
///    只在有 `.` 时才取末段，避免把 `ghostty` 这类平铺名字也切一刀。
fn is_terminal_name(name: &str) -> bool {
    let c = name.to_ascii_lowercase();
    if c.contains("terminal") || c.contains("retro-term") {
        return true;
    }
    if TERMINAL_CLASSES.contains(&c.as_str()) {
        return true;
    }
    match c.rsplit_once('.') {
        Some((_, last)) => TERMINAL_CLASSES.contains(&last),
        None => false,
    }
}

/// 读取焦点窗口链路第一个带 WM_CLASS 的窗口：返回 (窗口 id, 类名列表)。
/// 窗口 id 用于"回车前焦点是否变过"的比对——取带 WM_CLASS 的那一层（客户端
/// 窗口）而不是 X 的原始 focus，因为后者常是终端/编辑器的内部子窗口。
/// 返回 None 表示当前没有可识别的焦点窗口（如焦点在根窗口）。
#[cfg(target_os = "linux")]
fn focus_window_id_and_classes() -> Result<Option<(u32, Vec<String>)>, String> {
    use x11rb::protocol::xproto::{get_property, intern_atom, AtomEnum, ConnectionExt};
    // GetInputFocus 中 focus 为 0/1 分别表示 NONE / POINTER_ROOT（X 协议固定值）
    const POINTER_ROOT: u32 = 1;

    let (conn, _screen) = x11rb::connect(None).map_err(|e| format!("X11 connect: {e}"))?;
    let focus = conn
        .get_input_focus()
        .map_err(|e| format!("GetInputFocus: {e}"))?
        .reply()
        .map_err(|e| format!("GetInputFocus reply: {e}"))?
        .focus;

    let wm_class = intern_atom(&conn, false, b"WM_CLASS")
        .map_err(|e| format!("InternAtom: {e}"))?
        .reply()
        .map_err(|e| format!("InternAtom reply: {e}"))?
        .atom;

    let mut win = focus;
    for _ in 0..MAX_WINDOW_TRAVERSAL {
        if win == x11rb::NONE || win == POINTER_ROOT {
            return Ok(None);
        }
        let prop = get_property(&conn, false, win, wm_class, AtomEnum::STRING, 0, 256)
            .map_err(|e| format!("GetProperty: {e}"))?
            .reply()
            .map_err(|e| format!("GetProperty reply: {e}"))?;
        if !prop.value.is_empty() {
            // WM_CLASS 形如 "res_name\0res_class\0"
            let classes = prop
                .value
                .split(|&b| b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>();
            return Ok(Some((win, classes)));
        }
        let tree = conn
            .query_tree(win)
            .map_err(|e| format!("QueryTree: {e}"))?
            .reply()
            .map_err(|e| format!("QueryTree reply: {e}"))?;
        if tree.parent == x11rb::NONE || tree.parent == win {
            return Ok(None);
        }
        win = tree.parent;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回车余量必须是"刻意宽松但有界"的：短句只取基准值，
    /// 长文本递增但有上限，且整体不超过 1 秒（人对这个量级无感知）。
    #[test]
    fn test_enter_delay_is_conservative_and_bounded() {
        assert_eq!(enter_delay(0), ENTER_BASE_DELAY);
        assert_eq!(enter_delay(ENTER_EXTRA_MS_PER_CHARS - 1), ENTER_BASE_DELAY);
        assert_eq!(
            enter_delay(ENTER_EXTRA_MS_PER_CHARS),
            ENTER_BASE_DELAY + Duration::from_millis(1)
        );

        let worst = enter_delay(usize::MAX / 2);
        assert_eq!(worst, ENTER_BASE_DELAY + ENTER_EXTRA_MAX_DELAY);
        assert!(worst < Duration::from_millis(1000));
    }

    #[test]
    fn test_enter_delay_monotonic() {
        assert!(enter_delay(0) <= enter_delay(1_000));
        assert!(enter_delay(1_000) <= enter_delay(1_000_000));
    }

    /// 后端判定矩阵：Wayland 会话只看"合成器有没有给虚拟输入协议"，
    /// X11 会话只看有没有 DISPLAY，两个条件都不满足就是不支持。
    #[test]
    fn test_injection_backend_matrix() {
        use InjectionBackend::*;

        // Wayland 会话：协议在 ⇒ 能注入；协议不在（GNOME/KDE）⇒ 不支持
        assert_eq!(decide_backend(true, true, true), WlrWayland);
        assert_eq!(decide_backend(true, true, false), WlrWayland);
        assert_eq!(decide_backend(true, false, true), Unsupported);
        assert_eq!(decide_backend(true, false, false), Unsupported);

        // 非 Wayland 会话（X11 / Windows / macOS）
        assert_eq!(decide_backend(false, false, true), Native);
        assert_eq!(decide_backend(false, true, true), Native);
        assert_eq!(decide_backend(false, false, false), Unsupported);
    }

    /// 关键回归：Hyprland 这类"Wayland 会话 + 有 DISPLAY（XWayland 在跑）"
    /// 绝不能被当成 X11 处理——XTest 注不到原生 Wayland 客户端，这正是旧实现
    /// 把整个 Wayland 判为不支持的由来。
    #[test]
    fn test_xwayland_display_does_not_masquerade_as_x11() {
        let backend = decide_backend(true, true, true);
        assert_ne!(backend, InjectionBackend::Native);
        assert_eq!(backend, InjectionBackend::WlrWayland);
    }

    /// Wayland 会话用来掐掉 X11 后端的那个假 display 名必须是"连不上"的：
    /// 没有 ':' ⇒ x11rb 解析就失败，enigo 只保留虚拟键盘后端（实测如此）。
    /// 一旦有人把它改成看起来合法的名字，就会两个后端同时生效 ⇒ Ctrl+V 发两遍。
    #[cfg(target_os = "linux")]
    #[test]
    fn test_x11_disabled_display_is_unparseable() {
        assert!(
            !X11_DISABLED_DISPLAY.contains(':'),
            "Wayland 会话下这个假 display 名必须解析失败，否则会重复注入"
        );
    }

    /// 自动化环境里可能真的没有 Wayland/显示：探测函数只要求"不 panic、给出布尔"，
    /// 行为是否正确由上面的判定矩阵覆盖。
    #[test]
    fn test_injection_backend_is_total() {
        let _ = injection_backend();
    }
    /// 终端识别要同时认两种命名：X11 的 WM_CLASS（`xterm`/`kitty`）与 Wayland 的
    /// app_id（`foot`，或反向域名 `com.mitchellh.ghostty`）。
    #[test]
    fn test_is_terminal_name() {
        // 两条平台各来一批真名
        for name in [
            "xterm",
            "kitty",
            "foot",
            "ghostty",                    // X11 WM_CLASS
            "com.mitchellh.ghostty",      // Wayland app_id（反向域名）
            "org.wezfurlong.wezterm",
            "app.devsuite.Ptyxis",
            "com.raggesilver.BlackBox",
            "org.gnome.Terminal",         // 靠 "terminal" 命中
            "deepin-terminal",
            "xfce4-terminal",
            "cool-retro-term",
            "XTERM",                      // 大小写不敏感
        ] {
            assert!(is_terminal_name(name), "{name} 应识别为终端");
        }

        // 反面：普通应用、以及"长得像"的陷阱
        for name in [
            "zcode",
            "microsoft-edge",
            "com.mitchellh.ghostty-settings", // 末段不是清单里的名字
            "org.gnome.Nautilus",
            "football",                       // 平铺名字只认精确匹配，不做子串
            "com.example.termfmt",
            "",
        ] {
            assert!(!is_terminal_name(name), "{name} 不应识别为终端");
        }
    }

    /// 粘贴路由：只有"Wayland + 终端"这一格交给 wtype（enigo 在 Wayland 下发不出
    /// Shift）；X11 终端与其它情况仍走 enigo，非终端一律不带 Shift。
    #[test]
    fn test_paste_plan_matrix() {
        use InjectionBackend::*;
        assert_eq!(
            paste_plan(WlrWayland, true),
            PastePlan::WtypeTerminal,
            "Wayland 终端必须交给 wtype，否则 Shift 到不了终端"
        );
        assert_eq!(paste_plan(WlrWayland, false), PastePlan::Enigo { with_shift: false });
        assert_eq!(paste_plan(Native, true), PastePlan::Enigo { with_shift: true });
        assert_eq!(paste_plan(Native, false), PastePlan::Enigo { with_shift: false });
        assert_eq!(paste_plan(Unsupported, true), PastePlan::Enigo { with_shift: true });
    }

    /// 图片余量：小图只取基准值，大图按体积递增但有上限，整体不超过 1 秒；
    /// 且必须严格大于基准值的一半（太小就挡不住覆盖——这是这个闸门存在的理由）。
    #[test]
    fn test_image_settle_delay_is_conservative_and_bounded() {
        assert_eq!(image_settle_delay(0), IMAGE_SETTLE_BASE);
        assert_eq!(image_settle_delay(99 * 1024), IMAGE_SETTLE_BASE);
        assert_eq!(
            image_settle_delay(100 * 1024),
            IMAGE_SETTLE_BASE + Duration::from_millis(10)
        );
        assert!(image_settle_delay(4 * 1024 * 1024) > IMAGE_SETTLE_BASE);

        let worst = image_settle_delay(usize::MAX / 2);
        assert_eq!(worst, IMAGE_SETTLE_BASE + IMAGE_SETTLE_MAX_EXTRA);
        assert!(worst <= Duration::from_millis(1000));
        assert!(worst >= Duration::from_millis(300));
    }

    #[test]
    fn test_image_settle_delay_monotonic() {
        assert!(image_settle_delay(0) <= image_settle_delay(200 * 1024));
        assert!(image_settle_delay(200 * 1024) <= image_settle_delay(8 * 1024 * 1024));
    }

    /// 图片消化时长：小图只取基准值，按体积线性递增，且必须**明显长于**实测吞吐
    /// （1.83MB 在 ZCode 上要 2.1s ⇒ 不能给出比这更短的预算），否则回车会继续撞上
    /// "应用还在上传"的窗口。
    #[test]
    fn test_image_digest_delay_covers_measured_ingest() {
        assert_eq!(image_digest_delay(0), IMAGE_DIGEST_BASE);
        assert_eq!(
            image_digest_delay(1_000_000),
            IMAGE_DIGEST_BASE + IMAGE_DIGEST_PER_MP
        );
        // 2026-10-21 实测：1920x1440（2.76MP）从 Ctrl+V 到目标应用 commit 是 **1.32s**
        // （读剪贴板 + 上传两段之和）。预算必须盖住这个端到端值，否则回车会提前
        // ——第一版用 300ms/百万像素（只算了上传那一段）就在真机上翻车了
        assert!(
            image_digest_delay(2_764_800) >= Duration::from_millis(1320),
            "1920 长边的照片实测端到端 1.32s，预算不能比它短"
        );
        assert!(image_digest_delay(1000) <= image_digest_delay(20_000_000));
    }

    /// 消化队列：串行累加。第二张图若在上一张还没消化完时到达，它的起点顺延；
    /// 隔得够久就是"从这一刻重新开始"。
    #[test]
    fn test_queue_image_digest_is_serial() {
        let t = Instant::now();
        let one_mp = 1_000_000;

        // 队列空：从粘贴时刻开始算
        let after_first = queue_image_digest(None, t, one_mp);
        assert_eq!(after_first, t + image_digest_delay(one_mp));

        // 第二张在第一张消化完之前到达 ⇒ 顺延，而不是覆盖
        let second_pasted = t + Duration::from_millis(100);
        let after_second = queue_image_digest(Some(after_first), second_pasted, one_mp);
        assert_eq!(after_second, after_first + image_digest_delay(one_mp));
        assert_eq!(
            after_second - t,
            image_digest_delay(one_mp) + image_digest_delay(one_mp)
        );

        // 队列已经清空 ⇒ 不影响这一次
        let late = t + Duration::from_secs(60);
        assert_eq!(
            queue_image_digest(Some(after_second), late, one_mp),
            late + image_digest_delay(one_mp)
        );
    }

    /// **纯文字不受影响**：没贴过图 ⇒ 永远不睡；贴过但已过期 ⇒ 也不睡。
    /// 这条是产品对用户的承诺，钉成回归测试。
    #[test]
    fn test_remaining_digest_wait_is_none_for_text_only() {
        let t = Instant::now();
        assert_eq!(remaining_digest_wait(None, t), None, "没贴过图不能等");
        assert_eq!(
            remaining_digest_wait(Some(t - Duration::from_millis(1)), t),
            None,
            "图片早就消化完了，不能再等"
        );
        assert_eq!(
            remaining_digest_wait(Some(t), t),
            None,
            "正好等于当下算已就绪"
        );
    }

    /// 还要等：给剩余时长；超大图/超长队列一律封顶，不把发送无限推迟。
    #[test]
    fn test_remaining_digest_wait_is_bounded() {
        let t = Instant::now();
        assert_eq!(
            remaining_digest_wait(Some(t + Duration::from_millis(120)), t),
            Some(Duration::from_millis(120))
        );
        assert_eq!(
            remaining_digest_wait(Some(t + Duration::from_secs(60)), t),
            Some(IMAGE_DIGEST_MAX_WAIT),
            "上限必须生效"
        );
        assert!(IMAGE_DIGEST_MAX_WAIT <= Duration::from_secs(4));
    }

    /// 终端自动粘贴的可用性：只有"Wayland + 没有 wtype"这一格不ready
    #[test]
    fn test_terminal_paste_ready_matrix() {
        assert!(terminal_paste_ready_for(InjectionBackend::WlrWayland, true));
        assert!(!terminal_paste_ready_for(InjectionBackend::WlrWayland, false));
        // 其它后端不看 wtype
        assert!(terminal_paste_ready_for(InjectionBackend::Native, false));
        assert!(terminal_paste_ready_for(InjectionBackend::Unsupported, false));
    }

    /// 回车前的安全闸门：只有"前后都读到了、且不一样"才跳过发送；
    /// 读不到就按原行为照常发（fail-open，别因为检查不了就少发消息）。
    #[test]
    fn test_should_skip_enter_matrix() {
        let a = Some("wayland:com.mitchellh.ghostty|~/code".to_string());
        let b = Some("wayland:zcode|ZCode".to_string());
        assert!(should_skip_enter(a.clone(), b.clone()), "切了窗口就不该发回车");
        assert!(!should_skip_enter(a.clone(), a.clone()), "没切窗口照常发");
        assert!(!should_skip_enter(None, b.clone()), "读不到之前 ⇒ 照常发");
        assert!(!should_skip_enter(a.clone(), None), "读不到之后 ⇒ 照常发");
        assert!(!should_skip_enter(None, None), "两边都读不到 ⇒ 照常发");
    }
}
