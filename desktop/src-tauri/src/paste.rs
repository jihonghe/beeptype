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
use std::time::Duration;

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

/// 追加余量的上限，避免超长文本把回车拖得过久
const ENTER_EXTRA_MAX_DELAY: Duration = Duration::from_millis(500);

/// 回车前的等待时长：基准余量 + 按文本长度有界递增
fn enter_delay(text_chars: usize) -> Duration {
    let cap_ms = ENTER_EXTRA_MAX_DELAY.as_millis() as usize;
    let extra_ms = (text_chars / ENTER_EXTRA_MS_PER_CHARS).min(cap_ms);
    ENTER_BASE_DELAY + Duration::from_millis(extra_ms as u64)
}

/// WM_CLASS 精确命中这些值时视为终端（名字不含 "terminal" 的异类）。
/// 其余终端只要类名包含 "terminal" 即自动覆盖（deepin-terminal、
/// gnome-terminal、xfce4-terminal、terminator、tilix、ptyxis 等）。
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

/// 当前会话可用的注入后端（Wayland 下含一次合成器能力探测）
pub fn injection_backend() -> InjectionBackend {
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

/// 模拟 Enter 键发送（必须在 simulate_paste() 成功之后调用）。
///
/// 复用 simulate_paste() 交回的 enigo 实例，让粘贴与回车走同一条连接：
/// 同连接内的请求由 X server 按序处理，回车不会乱序跑到粘贴前面。
/// 调用线程阻塞 `enter_delay(text_chars)` + 按键耗时。
pub fn simulate_enter(enigo: &mut Enigo, text_chars: usize) -> Result<(), String> {
    std::thread::sleep(enter_delay(text_chars));
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
/// - Linux 终端窗口：Ctrl+Shift+V；
/// - 其余：Ctrl+V。
///
/// 必须在剪贴板写入成功之后调用；调用线程阻塞约 50ms。
/// 任何窗口检测失败都安全回退到 Ctrl+V。
/// 成功时交回 enigo 实例，供随后的自动发送复用同一条连接。
pub fn simulate_paste() -> Result<Enigo, String> {
    std::thread::sleep(PRE_PASTE_DELAY);

    let mut enigo = Enigo::new(&enigo_settings()).map_err(|e| format!("Enigo init: {e}"))?;

    #[cfg(target_os = "macos")]
    let result = {
        info!("Auto-paste shortcut: Cmd+V");
        send_paste(&mut enigo, Key::Meta, false)
    };

    #[cfg(not(target_os = "macos"))]
    let result = {
        // 终端识别靠 X11 焦点窗口，Wayland 下拿不到（合成器不对外暴露"谁在焦点"）
        // ⇒ 那里一律发 Ctrl+V，终端用户需要自己按终端的粘贴键
        #[cfg(target_os = "linux")]
        let terminal = if injection_backend() == InjectionBackend::WlrWayland {
            debug!("Terminal detection unavailable on Wayland; sending Ctrl+V");
            false
        } else {
            focus_window_is_terminal()
        };
        #[cfg(not(target_os = "linux"))]
        let terminal = false;
        info!("Auto-paste shortcut: {}", if terminal { "Ctrl+Shift+V" } else { "Ctrl+V" });
        send_paste(&mut enigo, Key::Control, terminal)
    };

    result?;
    Ok(enigo)
}

/// 当前焦点窗口（顶层）的 id，仅 Linux/X11 有值。
///
/// 用于注入回车前复核"用户没有切走窗口"。检测不到（非 Linux、焦点在根窗口、
/// X 连接失败）返回 None，调用方按"无从判断"处理，即保持原行为照常发送
/// （fail-open：宁可漏掉一次检查，也不因为检查不了而少发一条消息）。
pub fn focused_window_id() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        focus_window_id_and_classes().ok().flatten().map(|(id, _)| id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
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

/// 判断 X11 当前焦点窗口（或其祖先）是否属于终端应用。
#[cfg(target_os = "linux")]
fn focus_window_is_terminal() -> bool {
    match focus_window_id_and_classes() {
        Ok(Some((_, classes))) => classes.iter().any(|c| class_is_terminal(c)),
        Ok(None) => false,
        Err(e) => {
            warn!("Focus window detection failed, falling back to Ctrl+V: {e}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn class_is_terminal(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    c.contains("terminal") || c.contains("retro-term") || TERMINAL_CLASSES.contains(&c.as_str())
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
}

