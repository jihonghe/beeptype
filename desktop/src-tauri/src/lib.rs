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
use std::sync::{Arc, Mutex};
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

    // 窗口轮询（1s）时顺带续期：保证展示的二维码/配对码始终在有效期内
    server.ensure_code_fresh();
    let code = server.get_code();
    let content = qr_content(&ip, server.port, &code);
    let qr_svg = QrCode::new(content.as_bytes())
        .map(|qr| {
            qr.render::<svg::Color>()
                .min_dimensions(220, 220)
                .dark_color(svg::Color("#1a1a2e"))
                .light_color(svg::Color("#ffffff"))
                .build()
        })
        .unwrap_or_default();

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

/// 显示并聚焦主窗口（托盘菜单/托盘左键共用）。
/// view 指定这次唤起要落到的页面（"home" 主页 / "settings" 设置页）：每次唤起
/// 都是"重新进入"，不沿用上一次停留的页面与滚动位置（前端收到 view-request 后重置）。
/// 窗口是否在连接后自动收回由设置项「连接后自动隐藏窗口」决定（见 server.rs）。
fn show_main_window(app: &tauri::AppHandle, view: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        let _ = window.emit("view-request", view);
    }
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
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        // 托盘常驻：关闭按钮只隐藏窗口，真正退出走托盘菜单"退出"
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
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
            set_window_height
        ])
        .run(tauri::generate_context!())
        .expect("error while running BeepType application");
}
