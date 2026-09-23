//! X11 专属：**我们自己当剪贴板的主人**，用来避免内容进入剪贴板历史。
//!
//! ## 为什么需要这一层
//!
//! Windows/macOS 的剪贴板是"写进系统的一块共享内存"，给了就收不回来，只能靠**标记**
//! （`ExcludeClipboardContentFromMonitorProcessing` / `ConcealedType`）请求对方放过。
//! X11 不一样：剪贴板是**"所有权 + 按需提供"**协议——数据始终在我们进程的内存里，
//! 谁想要（SelectionRequest）都得来问我们，我们**给不给、给几次都由我们决定**。
//!
//! 于是可以做一件 Windows/macOS 做不到的事：**在"我们自己发出粘贴"之前，一律拒绝**
//! （剪贴板管理器都是在内容变化那一刻立刻来抓的，那时我们在场但不给 ⇒ 它的历史里没有）。
//! 粘贴之后放行 ⇒ 目标应用拿得到、用户之后手动 Ctrl+V 也照旧能用（兜底不丢）。
//!
//! ## 边界（都是有意为之，别当成 bug）
//!
//! - **只在 X11 会话启用**。Wayland 下剪贴板由合成器管，我们既当不了主人、也没有
//!   "能控制的粘贴时刻"来当闸门（用户是自己去按 Ctrl+V 的）⇒ 直接不启用。
//!   XWayland 场景同样排除：那时目标应用读的是原生 Wayland 剪贴板，我们写 X11 那边
//!   等于没写。
//! - **只在"本轮真的会发粘贴键"时才接管**（自动粘贴开着、平台支持、且我们自己窗口没焦点）。
//!   自动粘贴关着时没有"粘贴时刻"可当闸门，接管只会让用户手动 Ctrl+V 读到我们的内容 ⇒
//!   那种情况走普通写入（数据照旧在系统剪贴板里，等用户自己来取）。
//! - **接管后不做读回校验**：校验的目的是"发现写入被别的进程夺走/被静默丢弃"，
//!   而我们自己持有所有权时数据根本夺不走，校验无意义（也读不到——我们自己会拒绝读）。
//! - **我们进程退出，这份剪贴板内容就没了**（X11 的固有权衡：没有 owner 就没人提供数据）。
//!   对"说完即粘"的主流程无影响。
//! - **慢速轮询的剪贴板管理器**仍可能在放行窗口里抓到一次——挡不住，只能记日志。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use log::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// 会话判定（纯函数，便于单测）：是否该启用 X11 主人策略。
///
/// 只在**明确的 X11 会话**启用；`XDG_SESSION_TYPE` 缺失时才看 DISPLAY/WAYLAND_DISPLAY。
pub fn session_is_x11(
    session_type: Option<&str>,
    has_display: bool,
    has_wayland_display: bool,
) -> bool {
    match session_type {
        Some(t) => t.eq_ignore_ascii_case("x11"),
        None => has_display && !has_wayland_display,
    }
}

fn current_session_is_x11() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    session_is_x11(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var_os("DISPLAY").is_some(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
    )
}

/// 放行读取之后，是否该把数据交出去（纯函数，便于单测）。
pub fn should_serve(holding: bool, reads_allowed: bool) -> bool {
    holding && reads_allowed
}

enum Cmd {
    /// 接管并换成新内容（同时把放行闸门关回去）
    Take(String),
    /// 粘贴已发出：放行读取
    AllowReads,
}

static CLIP_TX: OnceLock<Option<Sender<Cmd>>> = OnceLock::new();

fn sender() -> Option<&'static Sender<Cmd>> {
    CLIP_TX
        .get_or_init(|| {
            if !current_session_is_x11() {
                debug!("clipboard: 非 X11 会话，不启用剪贴板主人策略");
                return None;
            }
            match spawn_owner_thread() {
                Ok(tx) => {
                    info!("clipboard: 已启用 X11 剪贴板主人策略（粘贴前拒绝读取，不入历史）");
                    Some(tx)
                }
                Err(e) => {
                    warn!("clipboard: X11 剪贴板主人策略启动失败，退回普通写入：{e}");
                    None
                }
            }
        })
        .as_ref()
}

/// 接管剪贴板并把内容换成 `text`（放行闸门先关回去）。
///
/// 返回 `true` 表示"交给我们了，调用方不必再走普通写入 + 读回校验"；
/// 返回 `false` 表示本策略不可用或失败——调用方按原路径处理（fail-open）。
pub fn take_ownership(text: &str) -> bool {
    match sender() {
        Some(tx) => tx.send(Cmd::Take(text.to_string())).is_ok(),
        None => false,
    }
}

/// 粘贴已发出：从此放行读取（目标应用拿得到，用户手动 Ctrl+V 也照旧可用）。
pub fn allow_reads() {
    if let Some(tx) = sender() {
        let _ = tx.send(Cmd::AllowReads);
    }
}

fn spawn_owner_thread() -> Result<Sender<Cmd>, String> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    thread::Builder::new()
        .name("clipboard-x11".to_string())
        .spawn(move || match Owner::new() {
            Ok(mut owner) => {
                let _ = ready_tx.send(Ok(()));
                owner.run(rx);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(tx),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("初始化超时：{e}")),
    }
}

struct Owner {
    conn: RustConnection,
    win: u32,
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    string: Atom,
    holding: bool,
    reads_allowed: bool,
    text_value: String,
}

impl Owner {
    fn new() -> Result<Self, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let screen = conn.setup().roots[screen_num].clone();
        let win = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            x11rb::COPY_FROM_PARENT as u8,
            win,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::NO_EVENT),
        )
        .map_err(|e| e.to_string())?;
        let atom = |name: &[u8]| -> Result<Atom, String> {
            conn.intern_atom(false, name)
                .map_err(|e| e.to_string())?
                .reply()
                .map(|r| r.atom)
                .map_err(|e| e.to_string())
        };
        Ok(Self {
            clipboard: atom(b"CLIPBOARD")?,
            targets: atom(b"TARGETS")?,
            utf8: atom(b"UTF8_STRING")?,
            text: atom(b"TEXT")?,
            string: AtomEnum::STRING.into(),
            conn,
            win,
            holding: false,
            reads_allowed: false,
            text_value: String::new(),
        })
    }

    fn take(&mut self, text: String) -> Result<(), String> {
        self.text_value = text;
        self.reads_allowed = false; // 新内容 = 新的隐私窗口
        self.conn
            .set_selection_owner(self.win, self.clipboard, x11rb::CURRENT_TIME)
            .map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())?;
        self.holding = true;
        debug!("clipboard: 接管剪贴板（{} 字），粘贴前不向任何人提供", self.text_value.chars().count());
        Ok(())
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Cmd::Take(t)) => {
                    if let Err(e) = self.take(t) {
                        warn!("clipboard: 接管失败，本次不再提供 X11 剪贴板：{e}");
                        self.holding = false;
                    }
                }
                Ok(Cmd::AllowReads) => {
                    self.reads_allowed = true;
                    debug!("clipboard: 放行读取（粘贴已发出）");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    debug!("clipboard: 主人线程退出（命令通道已关闭）");
                    return;
                }
            }
            // 处理 X 事件（拒绝/提供数据都在这里发生）
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(Event::SelectionRequest(req))) => self.on_request(req),
                    Ok(Some(Event::SelectionClear(_))) => {
                        // 别人接管了（用户复制了别的东西）：我们不再持有，也不再提供
                        if self.holding {
                            debug!("clipboard: 所有权已转移，停止提供数据");
                        }
                        self.holding = false;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        warn!("clipboard: X11 事件读取失败，主人策略退出：{e}");
                        return;
                    }
                }
            }
        }
    }

    fn on_request(&self, req: x11rb::protocol::xproto::SelectionRequestEvent) {
        if !should_serve(self.holding, self.reads_allowed) {
            // 关键：粘贴之前一律拒绝——剪贴板管理器就是在这个窗口里被抓空的
            debug!("clipboard: 拒绝一次粘贴前的读取（内容不进剪贴板历史）");
            self.refuse(&req);
            return;
        }
        if req.target == self.targets {
            // 列出我们支持的 target（有些客户端会先问这个）
            let atoms: Vec<u32> = vec![
                self.targets,
                self.utf8,
                self.text,
                self.string,
            ];
            if self.reply_atoms(&req, &atoms).is_err() {
                self.refuse(&req);
            }
            return;
        }
        if req.target == self.utf8 || req.target == self.text || req.target == self.string {
            let bytes = self.text_value.as_bytes().to_vec();
            if self.reply_bytes(&req, req.target, &bytes).is_err() {
                self.refuse(&req);
            }
            return;
        }
        // 不认识的类型：如实拒绝，别假装有
        self.refuse(&req);
    }

    fn reply_atoms(
        &self,
        req: &x11rb::protocol::xproto::SelectionRequestEvent,
        atoms: &[u32],
    ) -> Result<(), String> {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                req.requestor,
                req.property,
                AtomEnum::ATOM,
                atoms,
            )
            .map_err(|e| e.to_string())?;
        self.send_notify(req, req.property);
        Ok(())
    }

    fn reply_bytes(
        &self,
        req: &x11rb::protocol::xproto::SelectionRequestEvent,
        type_: Atom,
        data: &[u8],
    ) -> Result<(), String> {
        self.conn
            .change_property8(PropMode::REPLACE, req.requestor, req.property, type_, data)
            .map_err(|e| e.to_string())?;
        self.send_notify(req, req.property);
        Ok(())
    }

    fn refuse(&self, req: &x11rb::protocol::xproto::SelectionRequestEvent) {
        self.send_notify(req, x11rb::NONE);
    }

    fn send_notify(
        &self,
        req: &x11rb::protocol::xproto::SelectionRequestEvent,
        property: Atom,
    ) {
        let event = x11rb::protocol::xproto::SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property,
        };
        let _ = self
            .conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, event);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只在明确的_x11_会话启用() {
        assert!(session_is_x11(Some("x11"), true, false));
        assert!(!session_is_x11(Some("wayland"), true, true));
        // XWayland（wayland + DISPLAY）也不能启用：目标应用读的是 Wayland 剪贴板
        assert!(!session_is_x11(Some("wayland"), true, false));
        // 会话类型缺失时退而看环境
        assert!(session_is_x11(None, true, false));
        assert!(!session_is_x11(None, false, false));
        assert!(!session_is_x11(None, true, true));
    }

    #[test]
    fn 粘贴前一律不提供_粘贴后才放行() {
        assert!(!should_serve(true, false)); // 持有但还没粘贴 ⇒ 拒绝
        assert!(should_serve(true, true)); // 粘贴已发出 ⇒ 提供
        assert!(!should_serve(false, true)); // 已失去所有权 ⇒ 没什么可提供
    }

    /// 真机验证（需要真实 X11 会话，默认忽略；本地跑：
    /// `cargo test --lib -- --ignored --nocapture x11_`）。
    ///
    /// 它当场证明"粘贴前拒绝、粘贴后放行"真的生效——用 `xclip` 扮那个来抓数据的
    /// 剪贴板管理器。**会临时占用剪贴板**，跑完记得把用户原来的内容还回去。
    #[test]
    #[ignore = "需要真实 X11 会话（本地手动跑）"]
    fn x11_接管后粘贴前拒绝_放行后提供() {
        const PROBE: &str = "beep-type-clipboard-probe";
        assert!(take_ownership(PROBE), "接管应当成功（当前会话是 X11 吗？）");
        thread::sleep(Duration::from_millis(400));

        // 粘贴之前：来抓数据的应当被拒绝（xclip 拿不到内容）
        let before = std::process::Command::new("timeout")
            .args(["5", "xclip", "-selection", "clipboard", "-o"])
            .output();
        let got_before = before
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        assert_ne!(got_before, PROBE, "粘贴前不该提供数据（这正是挡历史的关键）");

        // 放行（真实场景里这一步紧跟在发出粘贴键之前）
        allow_reads();
        thread::sleep(Duration::from_millis(400));
        let after = std::process::Command::new("timeout")
            .args(["5", "xclip", "-selection", "clipboard", "-o"])
            .output()
            .expect("xclip 应当可执行");
        assert_eq!(
            String::from_utf8_lossy(&after.stdout).trim_end(),
            PROBE,
            "放行之后应当能正常读到内容（粘贴与手动 Ctrl+V 都靠它）"
        );
    }
}
