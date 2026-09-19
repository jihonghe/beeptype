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

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use log::{info, warn};
use std::time::Duration;

/// 写剪贴板后、发送快捷键前的等待：
/// 让 X11 剪贴板归属权切换被目标应用感知，避免它取到旧内容。
const PRE_PASTE_DELAY: Duration = Duration::from_millis(50);

/// 向上遍历窗口树的最大层数（焦点常落在终端内部的 VTE 子窗口上）
const MAX_WINDOW_TRAVERSAL: u32 = 20;

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

/// 向当前焦点窗口发送"粘贴"快捷键：
/// - macOS：Cmd+V；
/// - Linux 终端窗口：Ctrl+Shift+V；
/// - 其余：Ctrl+V。
///
/// 必须在剪贴板写入成功之后调用；调用线程阻塞约 50ms。
/// 任何窗口检测失败都安全回退到 Ctrl+V。
pub fn simulate_paste() -> Result<(), String> {
    std::thread::sleep(PRE_PASTE_DELAY);

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| format!("Enigo init: {e}"))?;

    #[cfg(target_os = "macos")]
    {
        send_paste(&mut enigo, Key::Meta, false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        let terminal = focus_window_is_terminal();
        #[cfg(not(target_os = "linux"))]
        let terminal = false;
        info!("Auto-paste shortcut: {}", if terminal { "Ctrl+Shift+V" } else { "Ctrl+V" });
        send_paste(&mut enigo, Key::Control, terminal)
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
    match detect_focus_classes() {
        Ok(Some(classes)) => classes.iter().any(|c| class_is_terminal(c)),
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

/// 读取焦点窗口链路的 WM_CLASS（res_name / res_class 两个字段）。
/// 返回 None 表示当前没有可识别的焦点窗口（如焦点在根窗口）。
#[cfg(target_os = "linux")]
fn detect_focus_classes() -> Result<Option<Vec<String>>, String> {
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
            return Ok(Some(classes));
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
