//! 自动粘贴：写好剪贴板后模拟系统快捷键，把文字送进当前焦点窗口的光标处。
//!
//! 平台策略：
//! - Linux X11：enigo 走 XTest 扩展（x11rb，纯 Rust），无需任何授权；
//!   并通过焦点窗口的 WM_CLASS 识别终端，终端发 Ctrl+Shift+V，其余发 Ctrl+V；
//! - Linux Wayland：安全模型禁止后台客户端模拟全局输入/无差别写剪贴板，
//!   除非走 RemoteDesktop 门户授权，v1 直接判定不支持，回退纯剪贴板；
//! - Windows：SendInput，无需授权（UIPI 隔离下无法输入到更高权限窗口），
//!   统一发 Ctrl+V（现代 Windows Terminal 也接受 Ctrl+V）；
//! - macOS：CGEvent，Cmd+V 全局一致（终端同样是 Cmd+V）。
//!
//! 时序策略（细节见 docs/TECH_DESIGN.md 第 10 节）：
//! - 粘贴快捷键与回车复用同一个 enigo 实例，同一条连接内由 X server 保证按序处理，
//!   不依赖墙钟时间排序；
//! - 注入回车前复核焦点窗口没变，变了就不发——避免把回车送给用户刚切过去的窗口；
//! - 粘贴与回车之间的等待是**安全余量**而不是性能参数：目标应用（尤其
//!   Chromium/Electron 系）读剪贴板是异步的，回车抢先执行就会变成空发送，
//!   而人对这段延迟无感知，所以取值刻意宽松，且不做按机器/按应用的特调。

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use log::{info, warn};
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

/// 当前会话是否支持全局按键模拟（自动粘贴）。
pub fn auto_paste_supported() -> bool {
    // 显式 Wayland 会话：不支持
    if std::env::var("XDG_SESSION_TYPE")
        .map(|t| t == "wayland")
        .unwrap_or(false)
    {
        return false;
    }
    // 无 X11 DISPLAY 的纯 Wayland 会话：不支持
    // （有 DISPLAY 说明 XWayland 可用，XTest 仍可工作）
    if cfg!(target_os = "linux")
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("DISPLAY").is_none()
    {
        return false;
    }
    true
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

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| format!("Enigo init: {e}"))?;

    #[cfg(target_os = "macos")]
    let result = {
        info!("Auto-paste shortcut: Cmd+V");
        send_paste(&mut enigo, Key::Meta, false)
    };

    #[cfg(not(target_os = "macos"))]
    let result = {
        #[cfg(target_os = "linux")]
        let terminal = focus_window_is_terminal();
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
}
