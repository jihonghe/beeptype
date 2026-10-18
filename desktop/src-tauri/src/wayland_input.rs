//! Wayland 会话下"合成器能不能收按键注入"的能力探测。
//!
//! 背景：Wayland 不允许任意客户端模拟全局输入，**但 wlroots 系合成器
//! （Hyprland、Sway）专门为这个场景暴露了虚拟输入协议**：
//!
//! - `zwp_virtual_keyboard_manager_v1`：合成器接收合成按键（wtype、enigo 都走它）
//! - `zwp_input_method_manager_v2`：合成器把客户端当作输入法，可直接提交文本
//!
//! GNOME 与 KDE 不暴露这两个协议（它们要么什么都不给，要么只走门户授权），
//! 所以那里的 Wayland 会话仍然只能回退"手动 Ctrl+V"。
//!
//! 判断方式：连上合成器读一遍全局接口列表，看有没有上述协议。
//! **刻意不去"试着建一个 enigo 实例"来探测**：那会真的向合成器新建虚拟键盘
//! 对象并下发一份键盘映射（有副作用），而这里纯读不写。

#[cfg(target_os = "linux")]
mod imp {
    use log::{debug, warn};
    use wayland_client::{
        globals::{registry_queue_init, GlobalListContents},
        protocol::wl_registry,
        Connection, Dispatch, QueueHandle,
    };

    /// 虚拟键盘协议：合成器接受合成的按下/抬起事件
    const VIRTUAL_KEYBOARD: &str = "zwp_virtual_keyboard_manager_v1";
    /// 输入法协议：合成器允许客户端直接提交文本
    const INPUT_METHOD: &str = "zwp_input_method_manager_v2";

    struct State;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    /// 当前 Wayland 合成器是否提供虚拟输入协议（wlroots 系为真）。
    ///
    /// 连不上合成器、或读注册表失败都按"没有"处理——探测不出能力时宁可
    /// 退化成手动粘贴，也不要报告一个可能做不到的能力（fail-safe）。
    pub fn virtual_input_available() -> bool {
        let conn = match Connection::connect_to_env() {
            Ok(conn) => conn,
            Err(e) => {
                debug!("Wayland capability probe: cannot connect to compositor: {e}");
                return false;
            }
        };
        let (globals, _queue) = match registry_queue_init::<State>(&conn) {
            Ok(pair) => pair,
            Err(e) => {
                warn!("Wayland capability probe failed, assuming no virtual input: {e}");
                return false;
            }
        };

        let found: Vec<String> = globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|g| g.interface)
            .filter(|name| name == VIRTUAL_KEYBOARD || name == INPUT_METHOD)
            .collect();
        debug!("Wayland virtual input protocols available: {found:?}");
        !found.is_empty()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    /// 非 Linux 没有这套协议（Windows/macOS 各走原生注入）
    pub fn virtual_input_available() -> bool {
        false
    }
}

pub use imp::virtual_input_available;
