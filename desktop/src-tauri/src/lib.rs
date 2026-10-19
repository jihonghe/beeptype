mod clipboard;
mod config;
mod discovery;
mod image_receiver;
mod paste;
mod protocol;
mod server;
mod wayland_input;
mod winicon;

use config::Config;
use log::info;
use qrcode::render::svg;
use qrcode::QrCode;
use serde::Serialize;
use server::{lan_ip, qr_content, serve, AppServerState, ConnState};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;

/// 前端轮询用的配对/状态信息（QR 以 SVG 字符串内联渲染，无需前端构建链）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingInfo {
    ip: String,
    port: u16,
    /// 连接中为空串，前端据此隐藏二维码区
    code: String,
    qr_svg: String,
    /// 常用设备席位是否已绑定（决定勾选框显隐）
    has_primary: bool,
    /// 已绑定常用设备的名称（设备管理区展示用）
    primary_name: Option<String>,
    connected: bool,
    connected_name: Option<String>,
    session_is_primary: bool,
}

/// 主窗口设置区状态
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInfo {
    /// 自动粘贴是否实际生效（用户开关 && 当前会话支持）
    auto_paste: bool,
    /// 自动发送是否实际生效（auto_paste 生效 && auto_send 开关 && 平台支持）
    auto_send: bool,
    /// 当前桌面会话是否支持全局按键模拟（GNOME/KDE 的 Wayland 等不支持）
    auto_paste_supported: bool,
    /// 当前会话是否支持自动发送（条件和 auto_paste 完全一致）
    auto_send_supported: bool,
    /// 手机连上后是否自动隐藏主窗口到托盘
    auto_hide: bool,
    /// 临时设备断线后的续连窗口（分钟；0 = 关闭）
    resume_ttl_min: u32,
    /// 开机自启是否开启（读系统实际状态）
    autostart: bool,
    /// 是否已绑定常用设备
    has_primary: bool,
    /// 已绑定常用设备名称
    primary_name: Option<String>,
    /// 终端里的自动粘贴是否可用（Wayland 缺 wtype 时为 false，设置页据此提示）
    terminal_paste_ready: bool,
    /// 应用版本：取自构建时的 tauri.conf.json（发布时由 CI 按 git tag 改写），
    /// 因此与安装包/发布产物一致。不要用 env!("CARGO_PKG_VERSION")——那是
    /// Cargo.toml 的 crate 版本，CI 不同步它，会一直显示旧号
    version: String,
}

/// 二维码 SVG：按内容缓存。
///
/// 前端每秒取一次配对信息，而二维码内容只在换码（约 3 分钟一次）或换 IP 时变；
/// 缓存前每秒都要重新做一次 QR 编码 + SVG 字符串渲染。
fn qr_svg_for(content: &str) -> String {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((key, svg)) = cache.as_ref() {
        if key == content {
            return svg.clone();
        }
    }
    let svg = QrCode::new(content.as_bytes())
        .map(|qr| {
            qr.render::<svg::Color>()
                .min_dimensions(220, 220)
                .dark_color(svg::Color("#1a1a2e"))
                .light_color(svg::Color("#ffffff"))
                .build()
        })
        .unwrap_or_default();
    *cache = Some((content.to_string(), svg.clone()));
    svg
}

fn build_pairing_info(server: &AppServerState) -> PairingInfo {
    let ip = lan_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    let (connected, connected_name, session_is_primary) = server.session_summary();
    let (has_primary, primary_name) = {
        let config = server.config.lock().unwrap();
        (
            config.has_primary(),
            config.primary.as_ref().map(|p| p.name.clone()),
        )
    };

    if connected {
        return PairingInfo {
            ip,
            port: server.port,
            code: String::new(),
            qr_svg: String::new(),
            has_primary,
            primary_name,
            connected: true,
            connected_name,
            session_is_primary,
        };
    }

    // 窗口可见时前端每秒取一次；隐藏时轮询已停，重新打开后的第一次取会顺带续期
    server.ensure_code_fresh();
    let code = server.get_code();
    let content = qr_content(&ip, server.port, &code);
    let qr_svg = qr_svg_for(&content);

    PairingInfo {
        ip,
        port: server.port,
        code,
        qr_svg,
        has_primary,
        primary_name,
        connected: false,
        connected_name: None,
        session_is_primary: false,
    }
}

/// 主窗口的创建参数。
///
/// tauri.conf.json 的 `app.windows` 是**空的**：窗口不再由 Tauri 在启动时预建，
/// 生命周期完全由代码管——启动建一次（见 setup），关闭按钮与"连接后自动收回"
/// 时**销毁**（见 on_window_event / server.rs 的 auto_hide_after_pairing），
/// 下次唤起再建（实测约 0.2s）。托盘常驻期间因此不存在 WebKit 渲染/网络进程
/// （两者合计约 150MB）。启动与唤起共用这一处参数，两边不会分叉。
fn create_main_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
        .title("BeepType")
        .inner_size(380.0, 520.0)
        .min_inner_size(340.0, 120.0)
        .resizable(true)
        .center()
        .build()
}

/// 重建窗口时"这次要落的页面"。
///
/// 新建的 webview 还没加载完，这时候发 `view-request` 一定丢——所以先把目标页面
/// 记在这里，前端加载完成后主动取走（`take_pending_view`）。
fn pending_view() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 前端启动时取走"这次唤起要落的页面"（取完即清空；没有则为 None，留在主页）
#[tauri::command]
fn take_pending_view() -> Option<String> {
    pending_view().lock().unwrap().take()
}

/// 销毁主窗口（关闭按钮 / 连接后自动收回共用），随后把我们自己的 WebKit 进程收掉。
///
/// 两步都不能省：
/// 1. **先 `Webview::close()` 再 `Window::destroy()`**：窗口销毁那条消息只摘掉窗口自身，
///    运行时里"这个窗口持有的 webview"还挂着 ⇒ GTK 部件不析构。症状：关窗后内存一点
///    没还，而且**每次唤起都新起一个渲染进程**（旧的那份还在）——来回几次内存翻倍。
/// 2. **再主动终止 WebKit 的渲染/网络进程**：WebKitGTK 的这两个进程是**跟着应用进程
///    活的**——真机上销毁窗口、乃至一个最小 GTK+WebKit 程序里销毁 webview 并释放全部
///    引用（gc 过），它们都不退（进程池在应用存活期间一直持有）。而它们合计约 150MB，
///    正是"关窗省内存"的大头。WebKit 本就是 out-of-process、按"进程随时可能死"设计，
///    下次建 webview 会自己起新进程——真机实测：关窗→收进程→再唤起 0.2s 打开，
///    页面与二维码都正常。
///
/// 只对本进程的子进程动手（/proc 里 ppid == 自己且 comm 以 WebKit 开头），
/// **不按名字全局杀**；收进程前还会再看一眼窗口是不是又被开出来了。
fn close_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        // WebviewWindow 实现了 AsRef<Webview>：拿它调 webview 级的 close，
        // 不能用 WebviewWindow::close——那个是"窗口级关闭"（会再发一次
        // CloseRequested，等于自己触发自己）
        let _ = window.as_ref().close();
        if let Err(e) = window.destroy() {
            log::warn!("Destroy main window failed: {e}");
        }
    }
    reap_webkit_children(app.clone());
}

/// 稍等片刻（让窗口销毁走完）再收掉 WebKit 的子进程。
/// 收之前复核一次"主窗口仍然不存在"：用户可能在几百毫秒内又把窗口开出来了，
/// 那时新起的进程不归这次收拾。
#[cfg(target_os = "linux")]
fn reap_webkit_children<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if app.get_webview_window("main").is_some() {
            return;
        }
        let victims = webkit_child_pids();
        if victims.is_empty() {
            return;
        }
        for &pid in &victims {
            // SAFETY: 只给本进程的子进程发信号（pid 来自 /proc 里的 ppid 比对）
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        std::thread::sleep(std::time::Duration::from_millis(800));
        let mut forced = 0;
        for &pid in &victims {
            if process_alive(pid) && webkit_child_pids().contains(&pid) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                forced += 1;
            }
        }
        info!(
            "Reaped {} WebKit child process(es) after closing the window{}{}",
            victims.len(),
            if forced > 0 {
                format!(" ({forced} forced)")
            } else {
                String::new()
            },
            " — they are spawned again on the next open"
        );
    });
}

/// 非 Linux 不做这件事：macOS 的 WKWebView 进程由系统按站点隔离管理、Windows 的
/// WebView2 进程跟着用户数据目录共享，都没有在这台机器上实测过——不猜着杀。
#[cfg(not(target_os = "linux"))]
fn reap_webkit_children<R: tauri::Runtime>(_app: tauri::AppHandle<R>) {}

/// 本进程的 WebKit 子进程（comm 前 15 字符会被截断，所以只比前缀）
#[cfg(target_os = "linux")]
fn webkit_child_pids() -> Vec<i32> {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        if !comm.trim().starts_with("WebKit") {
            continue;
        }
        // /proc/<pid>/stat 的 comm 字段带括号且可能含空格 ⇒ 从最后一个 ')' 之后切
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok());
        if ppid == Some(me) {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(target_os = "linux")]
fn process_alive(pid: i32) -> bool {
    // kill(pid, 0) 只探测存在性，不发信号
    unsafe { libc::kill(pid, 0) == 0 }
}

/// 显示并聚焦主窗口（托盘菜单/托盘左键/第二次启动共用）。窗口已被销毁时重建。
/// view 指定这次唤起要落到的页面（"home" 主页 / "settings" 设置页）：每次唤起
/// 都是"重新进入"，不沿用上一次停留的页面与滚动位置。
/// 窗口是否在连接后自动收回由设置项「连接后自动隐藏窗口」决定（见 server.rs）。
fn show_main_window(app: &tauri::AppHandle, view: &str) {
    let app = app.clone();
    let view = view.to_string();
    // 窗口只能在主线程创建，而调用方线程不固定（托盘菜单在主线程、单实例回调在
    // zbus 线程）⇒ 统一排到主线程执行
    let _ = app.clone().run_on_main_thread(move || {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
            let _ = window.emit("view-request", view);
            return;
        }
        *pending_view().lock().unwrap() = Some(view);
        match create_main_window(&app) {
            Ok(window) => {
                let _ = window.set_focus();
                winicon::apply_to_main_window();
            }
            Err(e) => log::error!("Create main window failed: {e}"),
        }
    });
}

#[tauri::command]
fn get_status(state: tauri::State<'_, Arc<Mutex<AppServerState>>>) -> String {
    let server = state.lock().unwrap();
    let conn_state = server.state.lock().unwrap();
    match &*conn_state {
        ConnState::Waiting => "等待配对".to_string(),
        ConnState::Connected => "已连接".to_string(),
    }
}

#[tauri::command]
fn get_pairing_info(
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) -> PairingInfo {
    let server = state.lock().unwrap();
    build_pairing_info(&server)
}

#[tauri::command]
fn set_remember_next(value: bool, state: tauri::State<'_, Arc<Mutex<AppServerState>>>) {
    let server = state.lock().unwrap();
    server.set_remember_next(value);
}

#[tauri::command]
fn disconnect_session(state: tauri::State<'_, Arc<Mutex<AppServerState>>>) {
    let server = state.lock().unwrap();
    server.disconnect_current();
}

#[tauri::command]
fn unbind_primary(state: tauri::State<'_, Arc<Mutex<AppServerState>>>) {
    let server = state.lock().unwrap();
    server.unbind_primary();
}

/// 读取设置区状态（自动粘贴生效值/平台支持/自动隐藏/开机自启）
#[tauri::command]
fn get_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) -> SettingsInfo {
    let server = state.lock().unwrap();
    let auto_paste = server.auto_paste.load(Ordering::Relaxed);
    let auto_send = server.auto_send.load(Ordering::Relaxed);
    let auto_hide = server.auto_hide.load(Ordering::Relaxed);
    let resume_ttl_min = server.config.lock().unwrap().resume_ttl_min;
    let (has_primary, primary_name) = {
        let config = server.config.lock().unwrap();
        (
            config.has_primary(),
            config.primary.as_ref().map(|p| p.name.clone()),
        )
    };
    drop(server);
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    SettingsInfo {
        auto_paste,
        auto_send,
        auto_paste_supported: paste::auto_paste_supported(),
        auto_send_supported: paste::auto_send_supported(),
        auto_hide,
        resume_ttl_min,
        autostart,
        has_primary,
        primary_name,
        terminal_paste_ready: paste::terminal_paste_ready(),
        version: app.package_info().version.to_string(),
    }
}

/// 按前端视图内容调整窗口高度（宽度保持用户当前值）。
/// 主页已连接态内容少，窗口收成 popover 高度；等待态/设置页更高。
#[tauri::command]
fn set_window_height(app: tauri::AppHandle, height: f64) {
    if let Some(win) = app.get_webview_window("main") {
        let width = match (win.inner_size(), win.scale_factor()) {
            (Ok(size), Ok(factor)) if factor > 0.0 => size.width as f64 / factor,
            _ => 380.0,
        };
        if let Err(e) = win.set_size(tauri::LogicalSize::new(width, height)) {
            log::warn!("set_size failed: {e}");
        }
    }
}

/// 切换自动粘贴：落盘、联动关自动发送，并推给在线的常用手机
/// （与手机端改开关走同一条 `apply_prefs`，两端行为不会分叉）
#[tauri::command]
fn set_auto_paste(
    value: bool,
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) {
    let server = state.lock().unwrap();
    server.apply_prefs(Some(value), None);
}

/// 切换自动发送：落盘 + 推给在线的常用手机（是否真生效还看平台能力与自动粘贴）
#[tauri::command]
fn set_auto_send(
    value: bool,
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) {
    let server = state.lock().unwrap();
    server.apply_prefs(None, Some(value));
}

/// 切换「连接后自动隐藏窗口」：手机连上后是否把主窗口收起到托盘。
/// 关掉时窗口一直留在桌面，便于盯状态、管理设备；不涉及任何平台能力判断。
#[tauri::command]
fn set_auto_hide(
    value: bool,
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) {
    let server = state.lock().unwrap();
    server.auto_hide.store(value, Ordering::Relaxed);
    let mut config = server.config.lock().unwrap();
    config.auto_hide = value;
    let _ = config.save();
    info!("Auto-hide toggled: {value}");
}

/// 把当前连接的临时手机设为常用设备（设置页「常用设备」按钮）。
/// 只在"还没有常用设备 + 当前是临时设备"时可用；失败原因会原样返回给界面。
#[tauri::command]
fn promote_session(state: tauri::State<'_, Arc<Mutex<AppServerState>>>) -> Result<(), String> {
    let server = state.lock().unwrap();
    server.promote_current_session()
}

/// 重启应用（托盘菜单调用）：先确认新进程能起来，再退出自己。
/// 失败返回错误，由调用方决定怎么提示（托盘用系统通知）。
fn restart_app_impl(app: &tauri::AppHandle) -> Result<(), String> {
    let exe = {
        let state = app.state::<Arc<Mutex<AppServerState>>>();
        let server = state.lock().unwrap();
        server.restart_self()?
    };
    info!("Restarting app (relaunch of {exe} scheduled)");
    app.exit(0);
    Ok(())
}

/// 设置临时设备的续连窗口（分钟；0 = 关闭连）。
/// 改完立即生效：窗口期在每次活动/续连时读取，已签发的凭证最长活到下一个刷新点。
#[tauri::command]
fn set_resume_ttl(
    minutes: u32,
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) {
    let server = state.lock().unwrap();
    {
        let mut config = server.config.lock().unwrap();
        config.resume_ttl_min = minutes;
        let _ = config.save();
    }
    info!("Transient keepalive TTL set to {minutes} min");
    // 改完立刻对在线的临时连接生效（开启就补发凭证、关闭就作废），
    // 不必等下次配对或降级
    server.sync_transient_keepalive();
}

/// 切换开机自启：操作系统自启项 + 持久化用户意愿
#[tauri::command]
fn set_autostart(
    app: tauri::AppHandle,
    value: bool,
    state: tauri::State<'_, Arc<Mutex<AppServerState>>>,
) -> Result<(), String> {
    let autolaunch = app.autolaunch();
    let result = if value {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    result.map_err(|e| format!("设置开机自启失败：{e}"))?;
    let server = state.lock().unwrap();
    let mut config = server.config.lock().unwrap();
    config.autostart = value;
    let _ = config.save();
    info!("Autostart toggled: {value}");
    Ok(())
}

/// Tauri 应用入口
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // enigo 的内部日志整条关掉：Wayland 会话下我们**故意**让它连不上 X11
    // （否则一次 Ctrl+V 会被注入两遍，见 paste.rs），而它为此每粘贴一次就打一行
    // `ERROR enigo::platform::x11] DisplayParsingError(...)`——那是我们自己的设计，
    // 不是异常，放在日志里只会让人误以为出了问题。注入真失败时由我们自己的
    // "Auto-paste failed: ..." 一行报告；需要看 enigo 内部日志时用 RUST_LOG=debug 覆盖
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,enigo=off"))
        .format_timestamp_millis()
        .init();

    tauri::Builder::default()
        // 第二次启动（菜单/命令行再点一次）＝唤起主窗口：窗口可能已被销毁，交给
        // show_main_window 决定是"显示已有的"还是"重新建一个"
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app, "home");
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        // 托盘常驻：关闭按钮 = **销毁窗口**（不是隐藏），真正退出走托盘菜单"退出"。
        // 销毁而不是隐藏是为了把那套 WebKit 进程还给系统（约 150MB），代价是下次
        // 唤起要重建 webview（实测约 0.2~0.3s）——托盘常驻的应用里这笔账划算。
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                close_main_window(window.app_handle());
            }
        })
        .setup(|app| {
            // 主窗口：启动即建（tauri.conf.json 的 app.windows 为空，窗口生命周期
            // 完全由代码管，见 create_main_window）
            let main_window = create_main_window(app.handle())?;
            let _ = main_window.set_focus();

            // 任务栏/dock 图标：tauri 的 window.set_icon() 在这套 GTK/tao 栈上实测
            // 无效（调用成功但 _NET_WM_ICON 属性不写），而缺这个属性的窗口在 Deepin
            // 等桌面的任务栏里会显示通用图标。改为自己把属性写上去（见 winicon.rs）。
            winicon::apply_to_main_window();

            // 加载配置（首次运行会补建设备 ID/设备名）
            let mut config = Config::load_or_create().unwrap_or_default();
            let port = config.port;
            let code = config.regenerate_code();
            let autostart_on = config.autostart;
            let auto_paste_config = config.auto_paste;
            let auto_send_config = config.auto_send;
            let auto_hide_config = config.auto_hide;
            let ip = lan_ip().unwrap_or_else(|| "127.0.0.1".to_string());
            info!("BeepType starting on {}:{} (device {})", ip, port, config.device_name);

            // 开机自启：以配置为准同步系统实际状态
            // （Linux 下为 ~/.config/autostart/BeepType.desktop，按 productName 命名）
            let autolaunch = app.autolaunch();
            if autostart_on {
                match autolaunch.enable() {
                    Ok(()) => info!("Autostart enabled"),
                    Err(e) => info!("Enable autostart failed: {e}"),
                }
            } else {
                match autolaunch.disable() {
                    Ok(()) => {}
                    Err(e) => info!("Disable autostart failed: {e}"),
                }
            }

            // 捕获本进程的可执行文件路径（重启功能要用；启动时装到内存，
            // 之后即使安装包被覆盖、路径变成 "(deleted)" 也不受影响）
            let exe_path = std::env::current_exe().ok();

            // 创建服务端状态
            let paste_supported = paste::auto_paste_supported();
            let send_supported = paste::auto_send_supported();
            info!(
                "Key injection backend: {} (this decides whether auto-paste is possible)",
                paste::injection_backend().label()
            );
            // Wayland 下"谁在焦点"只能靠合成器的顶层窗口协议（见 wayland_input.rs）：
            // 终端识别与回车前复核都要它，所以后端是 wlroots 时起一个常驻跟踪线程
            if paste::injection_backend() == paste::InjectionBackend::WlrWayland {
                wayland_input::start_focus_tracker();
            }
            info!("Auto-paste supported on this session: {paste_supported}");
            let effective_auto_paste = auto_paste_config && paste_supported;
            let effective_auto_send = auto_send_config && effective_auto_paste && send_supported;
            info!("Auto-send supported: {send_supported}, effective: {effective_auto_send}");
            if !paste::terminal_paste_ready() {
                info!(
                    "Terminal auto-paste unavailable: wtype not found on PATH \
                     (terminals fall back to manual paste; see settings page)"
                );
            }
            let server_state = AppServerState::new(
                app.handle().clone(),
                port,
                code,
                effective_auto_paste,
                effective_auto_send,
                auto_hide_config,
                config,
            );
            *server_state.exe_path.lock().unwrap() = exe_path;
            let shared_state = Arc::new(Mutex::new(server_state.clone()));

            app.manage(shared_state.clone());

            // ---- 托盘菜单（精简）----
            // 全部管理操作已移入主窗口控制台；托盘只保留状态查看、开窗、
            // 断开当前手机（踢人快捷项，无需开窗）和退出。
            // 状态行必须保持 enabled：GTK 会把禁用菜单项整体去色渲染成灰色，
            // 连状态圆点 emoji 也会变灰。点击事件由 on_menu_event 的兜底分支吞掉。
            let status_item =
                MenuItem::with_id(app, "status", "⚪ 等待配对", true, None::<&str>)?;
            let show_qr_item = MenuItem::with_id(app, "show_qr", "打开主窗口", true, None::<&str>)?;
            let settings_item = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
            let disconnect_item =
                MenuItem::with_id(app, "disconnect", "断开当前手机", false, None::<&str>)?;
            let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
            let restart_item =
                MenuItem::with_id(app, "restart", "重启应用", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

            let menu = Menu::with_items(
                app,
                &[
                    &status_item,
                    &show_qr_item,
                    &settings_item,
                    &disconnect_item,
                    &separator,
                    &restart_item,
                    &quit_item,
                ],
            )?;

            let _tray = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("BeepType")
                .icon(
                    tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))
                        .expect("bundled app icon must parse"),
                )
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "show_qr" => show_main_window(app, "home"),
                    "settings" => show_main_window(app, "settings"),
                    "disconnect" => {
                        let state = app.state::<Arc<Mutex<AppServerState>>>();
                        let server = state.lock().unwrap();
                        server.disconnect_current();
                    }
                    "restart" => {
                        // 托盘没有界面可以提示，失败时用系统通知告知
                        if let Err(e) = restart_app_impl(app) {
                            log::error!("Restart from tray failed: {e}");
                            let _ = app
                                .notification()
                                .builder()
                                .title("BeepType")
                                .body(&format!("重启失败：{e}"))
                                .show();
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 部分 Linux 桌面环境下左键单击托盘=唤起主窗口
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle(), "home");
                    }
                })
                .build(app)?;

            // 把托盘状态项与"断开"项句柄交给服务端，供状态变化时刷新
            *server_state.status_item.lock().unwrap() = Some(status_item.clone());
            server_state.set_menu_items(Some(disconnect_item.clone()));

            // 启动 TCP 服务端（后台线程）
            let tcp_state = server_state.clone();
            std::thread::spawn(move || {
                serve(tcp_state);
            });

            // 启动凭证门控 UDP 发现服务（后台线程）
            let disc_state = server_state.clone();
            std::thread::spawn(move || {
                discovery::serve_discovery(disc_state);
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_pairing_info,
            set_remember_next,
            disconnect_session,
            unbind_primary,
            get_settings,
            set_auto_paste,
            set_auto_send,
            set_auto_hide,
            set_resume_ttl,
            set_autostart,
            promote_session,
            set_window_height,
            take_pending_view
        ])
        .build(tauri::generate_context!())
        .expect("error while building BeepType application")
        .run(|_app, event| {
            // 托盘常驻：**窗口全被销毁 ≠ 退出**。关闭按钮就是销毁窗口（见上），
            // 不拦住的话 Tauri 会在最后一个窗口消失时退出整个应用——托盘里的
            // 服务端也就跟着没了。
            // `code` 只有 `app.exit()` / `app.restart()` 才带值（托盘「退出」、
            // 「重启应用」走的就是它）⇒ 那时放行，其余一律留下。
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 二维码按内容缓存：同内容重复取要一致（第二次命中缓存），
    /// 换码或换 IP 后必须重新编码——二维码里带着地址，换网络时只认码会留下旧地址
    #[test]
    fn test_qr_svg_cache_hits_and_invalidates() {
        let a = qr_svg_for("beeptype://192.168.1.2:8765?code=aaa");
        let b = qr_svg_for("beeptype://192.168.1.2:8765?code=aaa");
        assert_eq!(a, b, "同内容应命中缓存、结果一致");
        assert!(a.contains("<svg"), "应渲染成 SVG");
        let c = qr_svg_for("beeptype://192.168.1.2:8765?code=bbb");
        assert_ne!(a, c, "换码后必须重新编码");
        let d = qr_svg_for("beeptype://192.168.1.9:8765?code=bbb");
        assert_ne!(c, d, "换 IP 后必须重新编码");
    }
}
