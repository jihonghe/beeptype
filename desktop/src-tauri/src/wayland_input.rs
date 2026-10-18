//! wlroots 系 Wayland 的能力与窗口信息。
//!
//! 两件事：
//!
//! 1. **能力探测**（`virtual_input_available`）：合成器有没有给虚拟输入协议
//!    （`zwp_virtual_keyboard_manager_v1` / `zwp_input_method_manager_v2`）。
//!    wlroots 系（Hyprland、Sway）有，GNOME/KDE 没有。
//!    判断方式：连上合成器读一遍全局接口列表。**刻意不去"试着建一个 enigo 实例"**
//!    来探测——那会真的向合成器新建虚拟键盘对象并下发一份键盘映射（有副作用），
//!    而这里纯读不写。
//!
//! 2. **焦点窗口跟踪**（`start_focus_tracker` / `current_focus`）：
//!    用 wlroots 的 `zwlr_foreign_toplevel_manager_v1` 持续跟踪"哪个顶层窗口
//!    处于激活（焦点）状态"，拿到它的 app_id 与标题。Wayland 没有 X11 那种
//!    "问 X server 谁在焦点"的接口，合成器只通过这个协议把窗口信息给客户端；
//!    Wayland 下的终端识别与"回车前复核焦点"都靠它。
//!
//! 边界：协议只有 wlroots 系提供 ⇒ 这两件事**只在 `WlrWayland` 后端下成立**。
//! 跟踪线程断线或读不到时一律返回 `None`，调用方按"无从判断"处理（fail-open）。

/// 当前处于焦点（激活）的顶层窗口
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedWindow {
    pub app_id: String,
    pub title: String,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::FocusedWindow;
    use log::{debug, info, warn};
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock, RwLock};
    use wayland_client::{
        backend::ObjectData,
        globals::{registry_queue_init, GlobalListContents},
        protocol::wl_registry,
        Connection, Dispatch, Proxy, QueueHandle,
    };
    use wayland_protocols_wlr::foreign_toplevel::v1::client::{
        zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
        zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
    };

    /// 虚拟键盘协议：合成器接受合成的按下/抬起事件
    const VIRTUAL_KEYBOARD: &str = "zwp_virtual_keyboard_manager_v1";
    /// 输入法协议：合成器允许客户端直接提交文本
    const INPUT_METHOD: &str = "zwp_input_method_manager_v2";
    /// wlroots 的顶层窗口协议（带 activated 状态）
    const TOPLEVEL_MANAGER: &str = "zwlr_foreign_toplevel_manager_v1";

    /// `State` 事件里表示"激活（焦点）"的那个字节值（协议固定编号）
    const ACTIVATED: u8 = zwlr_foreign_toplevel_handle_v1::State::Activated as u8;

    /// 启动时注册的跟踪器快照句柄
    fn slot() -> &'static OnceLock<Arc<RwLock<Option<FocusedWindow>>>> {
        static SLOT: OnceLock<Arc<RwLock<Option<FocusedWindow>>>> = OnceLock::new();
        &SLOT
    }

    /// 读当前焦点窗口（跟踪器没启动或还没拿到状态时是 `None`）
    pub fn current_focus() -> Option<FocusedWindow> {
        slot().get()?.read().ok()?.clone()
    }

    struct State;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _state: &mut Self,
            _registry: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    /// 列一遍全局接口，返回接口名列表（探测用）
    fn global_interfaces() -> Result<Vec<String>, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
        let (globals, _queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
        Ok(globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|g| g.interface)
            .collect())
    }

    /// 当前合成器是否提供虚拟输入协议（wlroots 系为真）。
    ///
    /// 连不上合成器、或读注册表失败都按"没有"处理——探测不出能力时宁可
    /// 退化成手动粘贴，也不要报告一个可能做不到的能力（fail-safe）。
    pub fn virtual_input_available() -> bool {
        let interfaces = match global_interfaces() {
            Ok(list) => list,
            Err(e) => {
                debug!("Wayland capability probe failed, assuming no virtual input: {e}");
                return false;
            }
        };
        let found: Vec<&String> = interfaces
            .iter()
            .filter(|name| *name == VIRTUAL_KEYBOARD || *name == INPUT_METHOD)
            .collect();
        debug!("Wayland virtual input protocols available: {found:?}");
        !found.is_empty()
    }

    // ---------- 焦点窗口跟踪 ----------

    #[derive(Default, Clone)]
    struct TopInfo {
        app_id: String,
        title: String,
        /// 该窗口最近一次上报的状态里带没带 activated
        activated: bool,
    }

    #[derive(Default)]
    struct Tracker {
        tops: HashMap<u32, TopInfo>,
        snapshot: Arc<RwLock<Option<FocusedWindow>>>,
        /// 协议对象必须活着，丢了就不再收到状态事件
        _manager: Option<ZwlrForeignToplevelManagerV1>,
    }

    impl Tracker {
        /// 状态一变就重算快照（读方只拿这一份拷贝，不关心协议细节）
        ///
        /// 焦点窗口由"谁的状态里带 activated"决定，而不是记一个全局 id：
        /// 合成器给每个窗口独立发状态，旧窗口的"失活"事件完全可能后到，
        /// 按全局 id 记账会被它清掉，导致焦点突然变 None（实测踩过）。
        fn publish(&self) {
            let focused = self
                .tops
                .values()
                .find(|t| t.activated)
                .map(|t| FocusedWindow {
                    app_id: t.app_id.clone(),
                    title: t.title.clone(),
                });
            if let Ok(mut guard) = self.snapshot.write() {
                *guard = focused;
            }
        }
    }

    impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Tracker {
        fn event(
            state: &mut Self,
            _mgr: &ZwlrForeignToplevelManagerV1,
            event: zwlr_foreign_toplevel_manager_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
                let id = toplevel.id().protocol_id();
                state.tops.entry(id).or_default();
            }
        }

        /// manager 的 `toplevel` 事件会**创建子对象**（每个窗口一个 handle）：
        /// wayland-rs 要求在这里给出它的 ObjectData 工厂，缺了会在收到该事件时
        /// panic（实测把跟踪线程直接打死、焦点永远读不到）。
        fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
            match opcode {
                zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => {
                    qh.make_data::<ZwlrForeignToplevelHandleV1, ()>(())
                }
                other => panic!("unexpected child opcode {other} on toplevel manager"),
            }
        }
    }

    impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Tracker {
        fn event(
            state: &mut Self,
            handle: &ZwlrForeignToplevelHandleV1,
            event: zwlr_foreign_toplevel_handle_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let id = handle.id().protocol_id();
            let entry = state.tops.entry(id).or_default();
            match event {
                zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
                zwlr_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
                zwlr_foreign_toplevel_handle_v1::Event::State { state: bytes } => {
                    entry.activated = bytes.iter().any(|b| *b == ACTIVATED);
                }
                zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                    state.tops.remove(&id);
                }
                _ => {}
            }
            state.publish();
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Tracker {
        fn event(
            _state: &mut Self,
            _registry: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    /// 启动焦点窗口跟踪线程（无协议时什么也不做，只记一行日志）。
    ///
    /// 常驻线程一直阻塞在事件队列上，收到状态变化就更新快照；注入路径只是
    /// 读那份快照，不阻塞、不新建连接。
    pub fn start_focus_tracker() {
        let snapshot = Arc::new(RwLock::new(None));
        let _ = slot().set(snapshot.clone());

        std::thread::spawn(move || {
            let conn = match Connection::connect_to_env() {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Focus tracker: cannot connect to compositor: {e}");
                    return;
                }
            };
            let (globals, mut queue) = match registry_queue_init::<Tracker>(&conn) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("Focus tracker: registry failed: {e}");
                    return;
                }
            };
            let qh = queue.handle();

            // registry_queue_init 已经派发过一轮 globals，事件回调收不到它们 ⇒ 直接按名字 bind
            let manager = globals
                .contents()
                .clone_list()
                .into_iter()
                .find(|g| g.interface == TOPLEVEL_MANAGER);
            let manager = match manager {
                Some(g) => globals.registry().bind::<ZwlrForeignToplevelManagerV1, _, _>(
                    g.name,
                    g.version.min(3),
                    &qh,
                    (),
                ),
                None => {
                    info!("Focus tracker: compositor has no {TOPLEVEL_MANAGER}; focus is unknown");
                    return;
                }
            };
            let mut tracker = Tracker {
                snapshot,
                _manager: Some(manager),
                ..Default::default()
            };
            // 常驻：阻塞等事件，收到就更新快照
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut tracker) {
                    warn!("Focus tracker stopped: {e}");
                    return;
                }
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::FocusedWindow;

    /// 非 Linux 没有这套协议（Windows/macOS 各走原生注入与各自的焦点查询）
    pub fn virtual_input_available() -> bool {
        false
    }

    pub fn start_focus_tracker() {}

    pub fn current_focus() -> Option<FocusedWindow> {
        None
    }
}

pub use imp::{current_focus, start_focus_tracker, virtual_input_available};
