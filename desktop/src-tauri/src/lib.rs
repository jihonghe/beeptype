mod config;
mod discovery;
mod paste;
mod protocol;
mod server;

use config::Config;
use log::info;
use qrcode::render::svg;
use qrcode::QrCode;
use serde::Serialize;
use server::{lan_ip, qr_content, serve, ConnState, ServerState};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;

/// 前端轮询用的配对/状态信息（QR 以 SVG 字符串内联渲染，无需前端构建链）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingInfo {
    ip: String,
    port: u16,
    /// 连接中为空串，前端据此隐藏二维码区
    code: String,
    qr_svg: String,
    /// 主力位是否已绑定（决定勾选框显隐）
    has_primary: bool,
    /// 已绑定主力手机的名称（设备管理区展示用）
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
    /// 当前桌面会话是否支持全局按键模拟（Wayland 等不支持）
    auto_paste_supported: bool,
    /// 开机自启是否开启（读系统实际状态）
    autostart: bool,
    /// 是否已绑定主力手机
    has_primary: bool,
    /// 已绑定主力机名称
    primary_name: Option<String>,
    /// 应用版本（编译期注入）
    version: &'static str,
}

fn build_pairing_info(server: &ServerState) -> PairingInfo {
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
/// 标记为"手动打开"：连接/重连成功后不再自动收回窗口。
fn show_main_window(app: &tauri::AppHandle) {
    {
        let state = app.state::<Arc<Mutex<ServerState>>>();
        state
            .lock()
            .unwrap()
            .manual_window
            .store(true, Ordering::Relaxed);
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        // 通知前端立即刷新一次，不必等下一个 1s 轮询周期
        let _ = window.emit("pairing-show", ());
    }
}

/// 窗口被点叉隐藏：清除"手动打开"标记，下次扫码配对恢复自动收回行为
fn mark_window_hidden(app: &tauri::AppHandle) {
    let state = app.state::<Arc<Mutex<ServerState>>>();
    state
        .lock()
        .unwrap()
        .manual_window
        .store(false, Ordering::Relaxed);
}

#[tauri::command]
fn get_status(state: tauri::State<'_, Arc<Mutex<ServerState>>>) -> String {
    let server = state.lock().unwrap();
    let conn_state = server.state.lock().unwrap();
    match &*conn_state {
        ConnState::Waiting => "等待配对".to_string(),
        ConnState::Connected => "已连接".to_string(),
    }
}

#[tauri::command]
fn get_pairing_info(
    state: tauri::State<'_, Arc<Mutex<ServerState>>>,
) -> PairingInfo {
    let server = state.lock().unwrap();
    build_pairing_info(&server)
}

#[tauri::command]
fn set_remember_next(value: bool, state: tauri::State<'_, Arc<Mutex<ServerState>>>) {
    let server = state.lock().unwrap();
    server.set_remember_next(value);
}

#[tauri::command]
fn disconnect_session(state: tauri::State<'_, Arc<Mutex<ServerState>>>) {
    let server = state.lock().unwrap();
    server.disconnect_current();
}

#[tauri::command]
fn unbind_primary(state: tauri::State<'_, Arc<Mutex<ServerState>>>) {
    let server = state.lock().unwrap();
    server.unbind_primary();
}

/// 读取设置区状态（自动粘贴生效值/平台支持/开机自启）
#[tauri::command]
fn get_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<Mutex<ServerState>>>,
) -> SettingsInfo {
    let server = state.lock().unwrap();
    let auto_paste = server.auto_paste.load(Ordering::Relaxed);
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
        auto_paste_supported: paste::auto_paste_supported(),
        autostart,
        has_primary,
        primary_name,
        version: env!("CARGO_PKG_VERSION"),
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

/// 切换自动粘贴：运行态存"用户开关 && 平台支持"，配置只存用户意愿
#[tauri::command]
fn set_auto_paste(
    value: bool,
    state: tauri::State<'_, Arc<Mutex<ServerState>>>,
) {
    let server = state.lock().unwrap();
    let effective = value && paste::auto_paste_supported();
    server.auto_paste.store(effective, Ordering::Relaxed);
    let mut config = server.config.lock().unwrap();
    config.auto_paste = value;
    let _ = config.save();
    info!("Auto-paste toggled: {value} (effective: {effective})");
}

/// 切换开机自启：操作系统自启项 + 持久化用户意愿
#[tauri::command]
fn set_autostart(
    app: tauri::AppHandle,
    value: bool,
    state: tauri::State<'_, Arc<Mutex<ServerState>>>,
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
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
                // 窗口被点叉收走：恢复"配对弹窗"身份，下次扫码后自动收回
                mark_window_hidden(window.app_handle());
            }
        })
        .setup(|app| {
            // 加载配置（首次运行会补建设备 ID/设备名）
            let mut config = Config::load_or_create().unwrap_or_default();
            let port = config.port;
            let code = config.regenerate_code();
            let autostart_on = config.autostart;
            let auto_paste_config = config.auto_paste;
            let ip = lan_ip().unwrap_or_else(|| "127.0.0.1".to_string());
            info!("BeeType starting on {}:{} (device {})", ip, port, config.device_name);

            // 开机自启：以配置为准同步系统实际状态
            // （Linux 下为 ~/.config/autostart/beetype.desktop）
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

            // 创建服务端状态
            let paste_supported = paste::auto_paste_supported();
            info!("Auto-paste supported on this session: {paste_supported}");
            let effective_auto_paste = auto_paste_config && paste_supported;
            let server_state =
                ServerState::new(app.handle().clone(), port, code, effective_auto_paste, config);
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
            let disconnect_item =
                MenuItem::with_id(app, "disconnect", "断开当前手机", false, None::<&str>)?;
            let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

            let menu = Menu::with_items(
                app,
                &[
                    &status_item,
                    &show_qr_item,
                    &disconnect_item,
                    &separator,
                    &quit_item,
                ],
            )?;

            let _tray = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("BeeType")
                .icon(
                    tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))
                        .expect("bundled app icon must parse"),
                )
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "show_qr" => show_main_window(app),
                    "disconnect" => {
                        let state = app.state::<Arc<Mutex<ServerState>>>();
                        let server = state.lock().unwrap();
                        server.disconnect_current();
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
                        show_main_window(tray.app_handle());
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
            set_autostart,
            set_window_height
        ])
        .run(tauri::generate_context!())
        .expect("error while running BeeType application");
}
