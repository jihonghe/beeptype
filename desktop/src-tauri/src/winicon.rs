//! X11 窗口图标：给主窗口直接写入 `_NET_WM_ICON`。
//!
//! 为什么需要自己写：tauri 的 `window.set_icon()` 在这套 GTK/tao 栈上**实测无效**
//! ——调用返回成功，但窗口的 `_NET_WM_ICON` 属性始终不写（立即调用、延后 2 秒调用
//! 都试过）。而没有该属性的窗口，在 Deepin 这类桌面的任务栏里会退回通用图标
//! （现象：显示"两个齿轮"）。同类对照：本机 Edge / ZCode 都设了这个属性，图标正常；
//! 没设的窗口都显示通用图标。
//!
//! 做法：连上 X server，从 `_NET_CLIENT_LIST` 里找 `_NET_WM_PID` 等于本进程、
//! 且 `WM_CLASS` 含 "beetype" 的那个窗口，把 128×128 图标按 `_NET_WM_ICON` 的
//! 格式（[宽, 高, ARGB...] 32 位 cardinal）一次性写上去。
//!
//! 边界：
//! - 仅 Linux/X11。Wayland 下该属性无意义（桌面按 app_id 关联 desktop 文件），
//!   Windows/macOS 由可执行文件图标承担。
//! - 窗口要处于映射状态才会出现在 `_NET_CLIENT_LIST` 里，所以带重试（最长约 5 秒）；
//!   全部失败只记日志，不影响主流程。

#[cfg(target_os = "linux")]
use log::{info, warn};
#[cfg(target_os = "linux")]
use std::time::Duration;

/// 重试次数与间隔：窗口从创建到被 WM 收进 `_NET_CLIENT_LIST` 需要一点时间
#[cfg(target_os = "linux")]
const ATTEMPTS: u32 = 10;
#[cfg(target_os = "linux")]
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// 在后台线程里给主窗口设置 `_NET_WM_ICON`（失败只记日志）
pub fn apply_to_main_window() {
    #[cfg(target_os = "linux")]
    std::thread::spawn(|| {
        for attempt in 1..=ATTEMPTS {
            match try_set_icon() {
                Ok(true) => {
                    info!("Set _NET_WM_ICON on main window (attempt {attempt})");
                    return;
                }
                Ok(false) => std::thread::sleep(RETRY_INTERVAL),
                Err(e) => {
                    warn!("Set _NET_WM_ICON failed: {e}");
                    return; // X 相关错误重试也没用（例如 Wayland 下没有可用的 X server）
                }
            }
        }
        warn!("Set _NET_WM_ICON gave up after {ATTEMPTS} attempts");
    });
}

#[cfg(target_os = "linux")]
fn try_set_icon() -> Result<bool, String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    // change_property32 是 wrapper 模块里的便利方法（与 xproto::ConnectionExt 同名）
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("X11 connect: {e}"))?;
    let root = conn.setup().roots[screen_num].root;

    let atom = |name: &[u8]| -> Result<u32, String> {
        x11rb::protocol::xproto::intern_atom(&conn, false, name)
            .map_err(|e| format!("InternAtom: {e}"))?
            .reply()
            .map_err(|e| format!("InternAtom reply: {e}"))
            .map(|r| r.atom)
    };
    let net_client_list = atom(b"_NET_CLIENT_LIST")?;
    let net_wm_pid = atom(b"_NET_WM_PID")?;
    let wm_class = atom(b"WM_CLASS")?;
    let net_wm_icon = atom(b"_NET_WM_ICON")?;

    let clients = conn
        .get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, 1024)
        .map_err(|e| format!("GetProperty(_NET_CLIENT_LIST): {e}"))?
        .reply()
        .map_err(|e| format!("GetProperty reply: {e}"))?
        .value32()
        .map(|it| it.collect::<Vec<u32>>())
        .unwrap_or_default();

    let our_pid = std::process::id();
    for win in clients {
        let pid = conn
            .get_property(false, win, net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut it| it.next()));
        if pid != Some(our_pid) {
            continue;
        }

        let classes = conn
            .get_property(false, win, wm_class, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.value).to_lowercase())
            .unwrap_or_default();
        if !classes.contains("beetype") {
            continue;
        }

        let data = icon_cardinals()?;
        conn.change_property32(PropMode::REPLACE, win, net_wm_icon, AtomEnum::CARDINAL, &data)
            .map_err(|e| format!("ChangeProperty(_NET_WM_ICON): {e}"))?
            .check()
            .map_err(|e| format!("ChangeProperty check: {e}"))?;
        conn.flush().map_err(|e| format!("flush: {e}"))?;
        return Ok(true);
    }

    Ok(false)
}

/// 把内置图标转成 `_NET_WM_ICON` 需要的 32 位数组：[宽, 高, 每像素 ARGB...]。
/// 用 128×128 而不是 512×512：属性要在一个 X 请求里发完，128² 约 65KB 稳妥。
#[cfg(target_os = "linux")]
fn icon_cardinals() -> Result<Vec<u32>, String> {
    let img = image::load_from_memory(include_bytes!("../icons/128x128.png"))
        .map_err(|e| format!("icon decode: {e}"))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let mut data = Vec::with_capacity(2 + (w * h) as usize);
    data.push(w);
    data.push(h);
    for px in img.pixels() {
        let [r, g, b, a] = px.0;
        data.push(
            ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
        );
    }
    Ok(data)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// 图标数据格式：[宽, 高, ARGB...]，长度必须自洽（Deepin 等桌面按这个格式读）
    #[test]
    fn test_icon_cardinals_shape() {
        let data = icon_cardinals().expect("内置图标应能解码");
        let (w, h) = (data[0], data[1]);
        assert_eq!((w, h), (128, 128));
        assert_eq!(data.len(), 2 + (w * h) as usize);
        // 至少有一个不透明像素（图标不会是全透明）
        assert!(data[2..].iter().any(|px| px >> 24 != 0));
    }
}
