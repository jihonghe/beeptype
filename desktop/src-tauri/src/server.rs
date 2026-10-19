use crate::config::Config;
use crate::protocol::{Capabilities, now_ms, Message, MAX_LINE_BYTES};
use log::{info, warn};
use std::io::{BufRead, BufReader, Write};
use std::process::Command;
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tauri::menu::MenuItem;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_notification::NotificationExt;

/// 连接超时参数
/// 读超时（秒）：心跳 5s 一次；踢人走 shutdown(Read) 唤醒，不受此粒度限制
const READ_TIMEOUT_SECS: u64 = 5;
/// 连接级心跳超时（秒）：连续 3 次心跳无响应即收尸
const HEARTBEAT_TIMEOUT_SECS: u64 = 15;

/// 席位持有者被踢原因（AtomicU8 取值）
pub const KICK_TAKEOVER: u8 = 1;
pub const KICK_DISCONNECT: u8 = 2;
/// 同一常用设备的旧僵尸连接被新连接替换
pub const KICK_REPLACED: u8 = 4;

fn kick_reason(k: u8) -> &'static str {
    match k {
        KICK_TAKEOVER => "owner_takeover",
        KICK_REPLACED => "replaced",
        _ => "owner_disconnect",
    }
}

/// 粗粒度连接状态（托盘/窗口用）
#[derive(Clone, Debug, PartialEq)]
pub enum ConnState {
    Waiting,
    Connected,
}

/// 当前唯一活动会话（"席位"）
pub struct ActiveSession {
    pub client_id: String,
    pub name: String,
    /// 本席位的权威身份（true = 常用设备）。同时被两端共享：
    /// 命令线程改它（解绑降级 / 设为常用），连接线程读它（续连窗口、心跳里回报身份）。
    /// 唯一真源，不额外复制一份 bool，避免"两边各存一个、忘了同步"。
    pub identity: Arc<AtomicBool>,
    /// 非 0 表示持有者被要求结束，连接线程读到后发 session_ended 并退出
    pub kick: Arc<AtomicU8>,
    /// 连接 socket 的克隆：踢人时 shutdown(Read) 唤醒阻塞中的 read_line，
    /// 让 session_ended 与席位释放立刻完成，而不用等对端下一条心跳（最长 5s）
    pub wake: Mutex<Option<TcpStream>>,
    /// 本连接的共享写句柄：身份变化要从命令线程往连接里推消息（见 SharedWriter）
    pub writer: Option<SharedWriter>,
    /// 该手机上报的硬件指纹（转正时写入常用设备记录，保住"重装后仍认得出"）
    pub hardware_id: Option<String>,
    /// 对端 IP（写进常用设备记录的 last_ip，仅供参考）
    pub ip: String,
}

impl ActiveSession {
    /// 建一个新席位，并把它的 kick/identity 标志交回调用方（供 `ctx.admit` 绑定）
    fn new(
        client_id: String,
        name: String,
        is_primary: bool,
        ctx: &ConnCtx,
    ) -> (Self, Arc<AtomicU8>, Arc<AtomicBool>) {
        let kick = Arc::new(AtomicU8::new(0));
        let identity = Arc::new(AtomicBool::new(is_primary));
        let session = Self {
            client_id,
            name,
            identity: identity.clone(),
            kick: kick.clone(),
            wake: Mutex::new(clone_wake(&ctx.wake)),
            writer: ctx.writer.clone(),
            hardware_id: ctx.hardware_id.clone(),
            ip: ctx.peer_ip.map(|ip| ip.to_string()).unwrap_or_default(),
        };
        (session, kick, identity)
    }
}

/// 置踢出标志并唤醒连接线程
fn kick_session(a: &ActiveSession, reason: u8) {
    a.kick.store(reason, Ordering::SeqCst);
    if let Ok(w) = a.wake.lock() {
        if let Some(s) = w.as_ref() {
            let _ = s.shutdown(Shutdown::Read);
        }
    }
}

/// 给席位持有者发 session_ended 并记录日志
/// 统一的发送入口：所有写入（连接线程的回复 + 命令线程的推送）都走这把锁，
/// 避免两条线程各持一个 socket 克隆并发写、把一条 JSON 行写交错。
type SharedWriter = Arc<Mutex<TcpStream>>;

/// 通过共享写句柄发一条消息；返回是否成功发出（失败通常意味着连接已断）
fn send(writer: &SharedWriter, msg: &Message) -> bool {
    let Ok(encoded) = msg.encode() else {
        return false;
    };
    let Ok(mut w) = writer.lock() else {
        return false;
    };
    if w.write_all(encoded.as_bytes()).is_err() {
        return false;
    }
    let _ = w.flush();
    true
}

fn send_session_ended(writer: &SharedWriter, name: &str, reason: u8) {
    let ended = Message::SessionEnded {
        reason: kick_reason(reason).to_string(),
    };
    let _ = send(writer, &ended);
    warn!("Session ended for {} ({})", name, kick_reason(reason));
}

/// 通知席位持有者：常用设备已解绑，连接降级为临时（会话保持）
fn send_session_downgraded(writer: &SharedWriter, name: &str, resume: Option<(String, u64)>) {
    let with_resume = resume.is_some();
    let _ = send(writer, &Message::session_downgraded(resume));
    info!(
        "Session downgraded to transient for {} (resume credential issued: {})",
        name, with_resume
    );
}

fn clone_wake(w: &Option<TcpStream>) -> Option<TcpStream> {
    w.as_ref().and_then(|s| s.try_clone().ok())
}

/// 单个 TCP 连接的线程局部上下文
struct ConnCtx {
    client_id: String,
    name: String,
    /// 是否已通过认证（码或 token）
    authed: bool,
    /// 已认证常用设备但席位被占，连接保持、等待 takeover
    waiting_takeover: bool,
    /// 本连接持有席位时对应的 kick 标志（用于与全局席位比对）
    kick: Arc<AtomicU8>,
    /// 本连接持有席位时对应的身份标志（与席位共享，命令线程改、这里读）
    identity: Arc<AtomicBool>,
    /// 本连接 socket 的克隆，admit 时存入席位供远端踢人唤醒
    wake: Option<TcpStream>,
    last_active: Instant,
    /// 收到 bye：回完 ack 即退出
    closing: bool,
    /// 本连接的共享写句柄（连接线程与命令线程共用）
    writer: Option<SharedWriter>,
    /// 对端 IP（建席位时记进 ActiveSession）
    peer_ip: Option<IpAddr>,
    /// hello 里上报的硬件指纹
    hardware_id: Option<String>,
    /// 图片分块接收状态（每个连接一个；连接断开即随 ctx 一起丢弃）
    image: crate::image_receiver::ImageReceiver,
}

impl Default for ConnCtx {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            name: String::new(),
            authed: false,
            waiting_takeover: false,
            kick: Arc::new(AtomicU8::new(0)),
            identity: Arc::new(AtomicBool::new(false)),
            wake: None,
            last_active: Instant::now(),
            closing: false,
            writer: None,
            peer_ip: None,
            hardware_id: None,
            image: crate::image_receiver::ImageReceiver::new(),
        }
    }
}

impl ConnCtx {
    fn admit(
        &mut self,
        client_id: String,
        name: String,
        kick: Arc<AtomicU8>,
        identity: Arc<AtomicBool>,
    ) {
        self.client_id = client_id;
        self.name = name;
        self.authed = true;
        self.waiting_takeover = false;
        self.kick = kick;
        self.identity = identity;
        self.last_active = Instant::now();
    }
}

/// 临时会话的续连记录（**仅内存**，只保留最近一条）。
///
/// 临时手机被 Android 杀进程后，凭配对时下发的 `resume_token` 在窗口期内免扫码续接；
/// PC 端只存哈希，且随进程退出而消失——不落盘、不积累访客凭证。
pub struct TransientResume {
    pub client_id: String,
    /// SHA-256(resume_token) 的 hex
    pub token_hash: String,
    pub name: String,
    /// 滑动窗口的到期时刻：任何活动（心跳/发文字/续连成功）都会往后推
    pub expires_at: Instant,
}

impl TransientResume {
    /// 是否已过期（窗口期按最后一次活动起算）
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    /// 凭证校验：client_id + token 哈希都命中，且未过期
    pub fn verify(&self, client_id: &str, token: &str, now: Instant) -> bool {
        !self.is_expired(now)
            && self.client_id == client_id
            && self.token_hash == crate::config::sha256_hex(token)
    }
}

/// 常用设备 token 认证通过后的席位仲裁结果（纯逻辑，便于单测）
#[derive(Debug, PartialEq)]
enum PrimaryArbitration {
    /// 席位空闲，直接接入
    Admit,
    /// 被同一常用设备的旧连接占着（僵尸重连），踢旧接新
    ReplaceSelf,
    /// 被临时方占用，保持连接等待 takeover
    WaitTakeover,
}

fn arbitrate_primary(current: Option<&ActiveSession>, client_id: &str) -> PrimaryArbitration {
    match current {
        None => PrimaryArbitration::Admit,
        Some(s) if s.client_id == client_id => PrimaryArbitration::ReplaceSelf,
        Some(_) => PrimaryArbitration::WaitTakeover,
    }
}

/// 服务端共享状态。
///
/// 对运行时泛型化（`R: Runtime`）是为了能在无显示器环境下用 tauri 的 mock 运行时
/// 起一个实例，从而对握手/席位/凭证这套状态机做**真实 TCP 连接**的集成测试
/// （见文件末尾的 integration_tests）。桌面应用固定用 `AppServerState = ServerState<Wry>`。
pub struct ServerState<R: Runtime> {
    pub state: Arc<Mutex<ConnState>>,
    pub app_handle: AppHandle<R>,
    pub port: u16,
    pub pairing_code: Arc<Mutex<String>>,
    /// 持久配置（device_id/device_name/primary）
    pub config: Arc<Mutex<Config>>,
    /// 唯一席位
    pub session: Arc<Mutex<Option<ActiveSession>>>,
    /// 配对窗口"记住此手机"勾选：对下一次码配对生效；已有常用设备时忽略
    pub remember_next: Arc<AtomicBool>,
    /// 托盘菜单第一行状态项（setup 建完托盘后注入）
    pub status_item: Arc<Mutex<Option<MenuItem<R>>>>,
    /// 托盘"断开当前手机"项（有会话时可点）
    pub disconnect_item: Arc<Mutex<Option<MenuItem<R>>>>,
    /// 自动粘贴开关（实际生效值：用户开关 && 平台支持）
    pub auto_paste: Arc<AtomicBool>,
    /// 自动发送开关（粘贴后模拟 Enter）。实际生效值：auto_paste 生效 && 平台支持
    pub auto_send: Arc<AtomicBool>,
    /// 手机连上后是否自动隐藏主窗口到托盘（用户设置项）
    pub auto_hide: Arc<AtomicBool>,
    /// 临时会话的续连记录（仅内存，只留最近一条；窗口期 0 = 关闭续连）
    pub transient_resume: Arc<Mutex<Option<TransientResume>>>,
    /// 本进程的可执行文件路径，**启动时捕获**（setup 里写入）。
    /// 重启功能必须用它而不是运行时再取：装新版本后 /proc/self/exe 会变成
    /// "… (deleted)"，那时再取就找不到可执行文件了。
    pub exe_path: Arc<Mutex<Option<PathBuf>>>,
}

impl<R: Runtime> Clone for ServerState<R> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            app_handle: self.app_handle.clone(),
            port: self.port,
            pairing_code: self.pairing_code.clone(),
            config: self.config.clone(),
            session: self.session.clone(),
            remember_next: self.remember_next.clone(),
            status_item: self.status_item.clone(),
            disconnect_item: self.disconnect_item.clone(),
            auto_paste: self.auto_paste.clone(),
            auto_send: self.auto_send.clone(),
            auto_hide: self.auto_hide.clone(),
            transient_resume: self.transient_resume.clone(),
            exe_path: self.exe_path.clone(),
        }
    }
}

/// 桌面应用实际使用的状态类型
pub type AppServerState = ServerState<tauri::Wry>;

impl<R: Runtime> ServerState<R> {
    pub fn new(
        app_handle: AppHandle<R>,
        port: u16,
        code: String,
        auto_paste: bool,
        auto_send: bool,
        auto_hide: bool,
        config: Config,
    ) -> Self {
        // 扫码默认就是临时设备：勾选"绑为常用设备"必须是用户的主动行为
        let remember_default = false;
        Self {
            state: Arc::new(Mutex::new(ConnState::Waiting)),
            app_handle,
            port,
            pairing_code: Arc::new(Mutex::new(code)),
            config: Arc::new(Mutex::new(config)),
            session: Arc::new(Mutex::new(None)),
            remember_next: Arc::new(AtomicBool::new(remember_default)),
            status_item: Arc::new(Mutex::new(None)),
            disconnect_item: Arc::new(Mutex::new(None)),
            auto_paste: Arc::new(AtomicBool::new(auto_paste)),
            auto_send: Arc::new(AtomicBool::new(auto_send)),
            auto_hide: Arc::new(AtomicBool::new(auto_hide)),
            transient_resume: Arc::new(Mutex::new(None)),
            exe_path: Arc::new(Mutex::new(None)),
        }
    }

    /// setup 建完托盘后注入菜单项句柄
    pub fn set_menu_items(&self, disconnect: Option<MenuItem<R>>) {
        *self.disconnect_item.lock().unwrap() = disconnect;
    }

    /// 同步托盘状态行与"断开"项可用性
    pub(crate) fn publish_state(&self) {
        let (new_state, label, connected) = {
            let session = self.session.lock().unwrap();
            match session.as_ref() {
                None => (ConnState::Waiting, "⚪ 等待配对".to_string(), false),
                Some(a) if a.identity.load(Ordering::SeqCst) => {
                    (ConnState::Connected, format!("🟢 {}", a.name), true)
                }
                Some(a) => (
                    ConnState::Connected,
                    format!("🟢 {}", a.name),
                    true,
                ),
            }
        };
        *self.state.lock().unwrap() = new_state;
        if let Ok(guard) = self.status_item.lock() {
            if let Some(item) = guard.as_ref() {
                let _ = item.set_text(label);
            }
        }
        if let Ok(guard) = self.disconnect_item.lock() {
            if let Some(item) = guard.as_ref() {
                let _ = item.set_enabled(connected);
            }
        }
    }

    // ---- 临时会话的续连记录 ----

    /// 当前续连窗口时长（读设置；可用环境变量覆盖，仅测试用）
    fn resume_ttl(&self) -> std::time::Duration {
        self.config.lock().unwrap().resume_ttl()
    }

    /// 为一次临时会话签发续连凭证：返回明文（只下发这一次），内存里只存哈希。
    /// 窗口期配置为 0（关闭续连）时不签发，返回 None。
    pub fn issue_transient_resume(&self, client_id: &str, name: &str) -> Option<(String, u64)> {
        let ttl = self.resume_ttl();
        if ttl.is_zero() {
            return None;
        }
        let token = crate::config::generate_token();
        let record = TransientResume {
            client_id: client_id.to_string(),
            token_hash: crate::config::sha256_hex(&token),
            name: name.to_string(),
            expires_at: Instant::now() + ttl,
        };
        // 只保留最近一条：一台电脑只有一个席位，不积累访客凭证
        *self.transient_resume.lock().unwrap() = Some(record);
        info!("Transient resume issued for {name} (ttl {}s)", ttl.as_secs());
        Some((token, ttl.as_secs()))
    }

    /// 校验续连凭证；命中则把窗口期往后推（滑动窗口）并返回该会话的名字
    pub fn verify_transient_resume(&self, client_id: &str, token: &str) -> Option<String> {
        let ttl = self.resume_ttl();
        let mut guard = self.transient_resume.lock().unwrap();
        let record = guard.as_mut()?;
        if !record.verify(client_id, token, Instant::now()) {
            return None;
        }
        let name = record.name.clone();
        if !ttl.is_zero() {
            record.expires_at = Instant::now() + ttl;
        }
        Some(name)
    }

    /// 续连会话仍有活动（心跳/发文字）：把窗口期往后推
    fn touch_transient_resume(&self, client_id: &str) {
        let ttl = self.resume_ttl();
        if ttl.is_zero() {
            return;
        }
        if let Ok(mut guard) = self.transient_resume.lock() {
            if let Some(record) = guard.as_mut() {
                if record.client_id == client_id && !record.is_expired(Instant::now()) {
                    record.expires_at = Instant::now() + ttl;
                }
            }
        }
    }

    /// 作废续连记录（托盘断开、手机端 bye 时调用）
    fn clear_transient_resume(&self, client_id: &str) {
        if let Ok(mut guard) = self.transient_resume.lock() {
            if guard
                .as_ref()
                .is_some_and(|r| r.client_id == client_id)
            {
                *guard = None;
                info!("Transient resume cleared for {client_id}");
            }
        }
    }

    /// 把「临时连接保活」的当前设置**当场**同步给在线的临时连接。
    ///
    /// 开启/调长 → 当场签发并推送凭证；关闭 → 当场作废并通知手机清掉本地记录。
    /// 没有这一步的话，设置只在"签发凭证那一刻"被读取，改了要等下次配对或降级才生效
    /// （用户实测反馈）。
    pub fn sync_transient_keepalive(&self) {
        let live = {
            let session = self.session.lock().unwrap();
            match session.as_ref() {
                // 只对"当前是临时身份"的在线会话同步
                Some(a) if !a.identity.load(Ordering::SeqCst) => {
                    Some((a.client_id.clone(), a.name.clone(), a.writer.clone()))
                }
                _ => None,
            }
        };
        let Some((client_id, name, writer)) = live else {
            return;
        };
        let ttl = self.resume_ttl();
        let grant = if ttl.is_zero() {
            self.clear_transient_resume(&client_id); // 关闭：立刻作废
            None
        } else {
            self.issue_transient_resume(&client_id, &name)
        };
        if let Some(w) = writer {
            let msg = match &grant {
                Some((token, secs)) => Message::resume_grant(Some(token), *secs),
                None => Message::resume_grant(None, 0),
            };
            let _ = send(&w, &msg);
        }
        info!(
            "Transient keepalive synced (ttl {}s, grant issued: {})",
            ttl.as_secs(),
            grant.is_some()
        );
    }

    /// 只读校验一张临时续连凭证（**不续期**），供 UDP 发现门控使用。
    ///
    /// 临时设备被系统杀掉后，手机只记得 IP+端口；电脑换了 IP 就再也找不回来。
    /// 让发现服务认得"窗口期内仍有效的临时凭证"，临时设备就能和常用设备一样
    /// 用广播找到换过 IP 的电脑。续期仍只发生在真正意义上的活动（心跳/续连成功）。
    pub fn transient_resume_matches(&self, client_id: &str, proof: &str) -> bool {
        let Ok(guard) = self.transient_resume.lock() else {
            return false;
        };
        guard.as_ref().is_some_and(|record| {
            !record.is_expired(Instant::now())
                && record.client_id == client_id
                && record.token_hash == proof
        })
    }

    /// 获取当前配对码
    pub fn get_code(&self) -> String {
        self.pairing_code.lock().unwrap().clone()
    }

    /// 更新配对码
    pub fn set_code(&self, code: String) {
        if let Ok(mut c) = self.pairing_code.lock() {
            *c = code;
        }
    }

    /// 等待配对期间保证码新鲜：已过期或剩余不足 2 分钟则自动换新。
    ///
    /// 配对码寿命只有 5 分钟，而 PC 往往长期运行、窗口按需唤起——
    /// 若不续期，窗口展示的二维码在启动 5 分钟后必然成为死码，
    /// 扫码只会收到「二维码已失效」，得重新回电脑前看码。已连接时绝不换码。
    pub fn ensure_code_fresh(&self) {
        if self.session_summary().0 {
            return;
        }
        const CODE_REFRESH_THRESHOLD_MS: u64 = 120_000;
        let need_renew = {
            let config = self.config.lock().unwrap();
            match config.code_expires_at {
                None => true,
                Some(expires) => expires.saturating_sub(now_ms()) < CODE_REFRESH_THRESHOLD_MS,
            }
        };
        if need_renew {
            let new_code = {
                let mut config = self.config.lock().unwrap();
                config.regenerate_code()
            };
            info!("Pairing code auto-refreshed (expired or near expiry)");
            self.set_code(new_code);
        }
    }

    // ---- 供 Tauri commands 调用的运行态操作 ----

    pub fn set_remember_next(&self, value: bool) {
        self.remember_next.store(value, Ordering::Relaxed);
    }

    /// 席位摘要：(是否连接中, 设备名, 是否常用设备)
    pub fn session_summary(&self) -> (bool, Option<String>, bool) {
        let s = self.session.lock().unwrap();
        match s.as_ref() {
            Some(a) => (true, Some(a.name.clone()), a.identity.load(Ordering::SeqCst)),
            None => (false, None, false),
        }
    }

    /// 托盘"断开当前手机"：通知持有者结束，席位释放由其连接退出时完成
    pub fn disconnect_current(&self) {
        let (client_id, name) = {
            let session = self.session.lock().unwrap();
            match session.as_ref() {
                Some(a) => {
                    kick_session(a, KICK_DISCONNECT);
                    (a.client_id.clone(), a.name.clone())
                }
                None => return,
            }
        };
        // 主人主动结束 = 这次临时访问到此为止：续连凭证一并作废，不能自己回来
        self.clear_transient_resume(&client_id);
        info!("Disconnect requested for {name} ({client_id})");
    }

    /// 设置页/托盘"解除绑定"：凭证立即作废；若常用设备正在线，连接降级为
    /// 临时设备继续服务（不断开）。降级通知**当场从这里发**，不靠连接线程轮询：
    /// 连接线程阻塞在 socket 读上，靠它发现标志要等最长 5 秒（下一次心跳/读超时），
    /// 且历史上那个"只发一次"的闩会让重复解绑彻底静默。写操作都在 writer 锁后面，
    /// 与连接线程的回复不会写交错（同 promote_current_session 的做法）。
    /// 手机端改「自动粘贴 / 自动发送」。
    ///
    /// **只有常用设备席位能改**：这两个开关改的是电脑的全局行为，临时访客不该有权限。
    /// 不合规的请求一律忽略——但**照样回推当前值**，让手机端自己把界面纠正回来
    /// （不做静默失败）。
    fn handle_set_prefs(&self, ctx: &mut ConnCtx, auto_paste: bool, auto_send: bool) -> Option<Message> {
        let allowed = ctx.authed
            && !ctx.waiting_takeover
            && self.is_session_holder(ctx)
            && ctx.identity.load(Ordering::SeqCst);
        if allowed {
            // 整对应用（落盘 + 回推一条 prefs + 通知电脑端自己的界面）
            self.apply_prefs(Some(auto_paste), Some(auto_send));
        } else {
            warn!("Ignored set_prefs from a non-primary connection");
            // 改不动也要让手机看到当前值：不做静默失败，界面不会停在假状态
            self.push_prefs();
        }
        // 回推已经在上面发过了：这里不再回一条重复的（否则手机要处理两条）
        None
    }

    /// 当前行为开关（用户意愿值）的消息形式
    fn prefs_message(&self) -> Message {
        let config = self.config.lock().unwrap();
        Message::prefs(config.auto_paste, config.auto_send)
    }

    /// 应用行为开关：写内存 + 落盘，并把结果推给在线的常用手机。
    ///
    /// 依赖关系与电脑端设置页完全一致：**auto_paste 关 → auto_send 必须同时关**
    /// （没东西可发，发送开关只能是关）。传 None 表示这一项不动。
    pub fn apply_prefs(&self, auto_paste: Option<bool>, auto_send: Option<bool>) {
        {
            let mut config = self.config.lock().unwrap();
            if let Some(v) = auto_paste {
                config.auto_paste = v;
                if !v {
                    config.auto_send = false;
                }
                self.auto_paste
                    .store(v && crate::paste::auto_paste_supported(), Ordering::Relaxed);
                // 联动必须**同时作用于运行标志**：电脑端设置页读的就是它，
                // 只关配置会出现"勾着但置灰"、再把粘贴打开时又变回开着（用户实测）。
                // 这里按当前配置重算一遍，幂等且不会漏。
                self.auto_send.store(
                    config.auto_send
                        && crate::paste::auto_send_supported()
                        && self.auto_paste.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
            }
            if let Some(v) = auto_send {
                let paste_on = config.auto_paste;
                config.auto_send = v && paste_on;
                self.auto_send.store(
                    config.auto_send
                        && crate::paste::auto_send_supported()
                        && self.auto_paste.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
            }
            let _ = config.save();
        }
        info!(
            "Prefs applied: auto_paste={} auto_send={}",
            self.auto_paste.load(Ordering::Relaxed),
            self.auto_send.load(Ordering::Relaxed)
        );
        self.push_prefs();
        // 电脑端自己的设置页也立刻刷新：事件推送，不再等 1 秒轮询
        let _ = self.app_handle.emit("settings-changed", ());
    }

    /// 把当前行为开关推给在线的常用手机。手机改完、电脑设置页改完、以及
    /// 手机刚连上（席位建立后）都会走这里，保证两端显示一致。
    pub fn push_prefs(&self) {
        let writer = {
            let session = self.session.lock().unwrap();
            match session.as_ref() {
                Some(a) if a.identity.load(Ordering::SeqCst) => a.writer.clone(),
                _ => None,
            }
        };
        if let Some(w) = writer {
            let _ = send(&w, &self.prefs_message());
        }
    }

    pub fn unbind_primary(&self) {
        // 锁顺序约定：config → session。先把 config 处理完，再去会话里取收件人，
        // 拿到就立刻放锁——签发凭证要读 config（resume_ttl），持着 session 锁会反向加锁
        self.config.lock().unwrap().unbind_primary();

        let live = {
            let session = self.session.lock().unwrap();
            match session.as_ref() {
                Some(a) if a.identity.load(Ordering::SeqCst) => {
                    a.identity.store(false, Ordering::SeqCst);
                    Some((a.client_id.clone(), a.name.clone(), a.writer.clone()))
                }
                _ => None,
            }
        };

        match live {
            Some((client_id, name, writer)) => {
                // 降级后的身份就是"临时设备"，凭证待遇必须与扫码临时设备一致：
                // 补发续连凭证，否则这条会话被系统杀掉后就再也回不来
                let grant = self.issue_transient_resume(&client_id, &name);
                if let Some(w) = writer {
                    send_session_downgraded(&w, &name, grant);
                }
                info!("Primary unbound; live connection downgraded to transient");
            }
            None => info!("Primary unbound; no live primary connection"),
        }
        self.publish_state();
    }

    /// 处理收到的消息行，返回需要回复的消息
    fn handle_message(&self, line: &str, peer_ip: IpAddr, ctx: &mut ConnCtx) -> Option<Message> {
        let msg = match Message::decode_line(line) {
            Ok(m) => m,
            Err(e) => {
                warn!("Parse error: {} | line: {} bytes", e, line.len());
                return None;
            }
        };

        match msg {
            Message::Hello {
                code,
                token,
                resume,
                client_id,
                device_name,
                hardware_id,
                has_local_credential,
            } => self.handle_hello(
                code,
                token,
                resume,
                client_id,
                device_name,
                hardware_id,
                has_local_credential,
                peer_ip,
                ctx,
            ),

            Message::SetPrefs {
                auto_paste,
                auto_send,
            } => self.handle_set_prefs(ctx, auto_paste, auto_send),

            // ── 图片：分块收齐后解码 → 写剪贴板 → 自动粘贴（**不回车**，见 handle_image）
            Message::ImageBegin {
                id,
                mime,
                total_bytes,
                chunks,
            } => {
                if let Some(reply) = self.image_guard(ctx, peer_ip, &id) {
                    return Some(reply);
                }
                ctx.last_active = Instant::now();
                match ctx.image.begin(&id, &mime, total_bytes, chunks) {
                    crate::image_receiver::ImageProgress::Continue => None,
                    crate::image_receiver::ImageProgress::Failed { id, code, message } => {
                        warn!("image_begin rejected: {message}");
                        Some(self.image_ack_fail(id, code))
                    }
                    crate::image_receiver::ImageProgress::Complete { .. } => None,
                }
            }

            Message::ImageChunk { id, seq, data } => {
                if !ctx.authed || ctx.waiting_takeover {
                    return None; // 数据块不回话，静默丢弃（真正的结果由 image_end 的 ack 给）
                }
                ctx.last_active = Instant::now();
                match ctx.image.chunk(&id, seq, &data) {
                    crate::image_receiver::ImageProgress::Continue => None,
                    crate::image_receiver::ImageProgress::Failed { id, code, message } => {
                        warn!("image_chunk rejected: {message}");
                        Some(self.image_ack_fail(id, code))
                    }
                    crate::image_receiver::ImageProgress::Complete { .. } => None,
                }
            }

            Message::ImageEnd { id } => {
                if let Some(reply) = self.image_guard(ctx, peer_ip, &id) {
                    return Some(reply);
                }
                ctx.last_active = Instant::now();
                match ctx.image.end(&id) {
                    crate::image_receiver::ImageProgress::Complete { id, mime, bytes } => {
                        self.handle_image(id, mime, bytes)
                    }
                    crate::image_receiver::ImageProgress::Failed { id, code, message } => {
                        warn!("image_end rejected: {message}");
                        Some(self.image_ack_fail(id, code))
                    }
                    crate::image_receiver::ImageProgress::Continue => None,
                }
            }

            Message::Text { id, text, ts: _ } => {
                // 必须是席位持有者；等待接管/未认证连接一律拒绝
                let is_holder = self.is_session_holder(ctx);
                if !ctx.authed || ctx.waiting_takeover || !is_holder {
                    warn!("Text from non-session connection {}", peer_ip);
                    return Some(Message::Ack {
                        id,
                        ok: false,
                        code: Some("not_connected".to_string()),
                        pasted: false,
                        srv_recv_ms: now_ms(),
                    });
                }
                ctx.last_active = Instant::now();
                // 临时设备仍在活跃：续连窗口往后推
                if !ctx.identity.load(Ordering::SeqCst) {
                    self.touch_transient_resume(&ctx.client_id);
                }
                self.handle_text(id, text)
            }

            Message::Ping => {
                ctx.last_active = Instant::now();
                if !ctx.identity.load(Ordering::SeqCst) {
                    self.touch_transient_resume(&ctx.client_id);
                }
                // 心跳顺带回报权威身份：手机据此自愈（推送万一丢了，5 秒内也能纠正）。
                // 只有真正持席的连接才给身份声明——等待接管的连接还没拿到席位，
                // 未认证连接更是无从谈起，一律不给（手机收到空身份就忽略）
                let holder = self.is_session_holder(ctx);
                let is_primary = ctx.identity.load(Ordering::SeqCst);
                let session = if holder {
                    if ctx.waiting_takeover {
                        None
                    } else if is_primary {
                        Some("primary".to_string())
                    } else {
                        Some("transient".to_string())
                    }
                } else {
                    None
                };
                // 临时接入再顺带同步一次窗口剩余时长：手机本地记的到期时间不会滑动，
                // 不同步它就会在窗口还没到时自以为过期（一断网就把记录丢掉）
                let ttl = if holder && !is_primary && !ctx.waiting_takeover {
                    Some(self.resume_ttl().as_secs())
                } else {
                    None
                };
                Some(Message::pong(session.as_deref()).with_resume_ttl(ttl))
            }

            Message::PromotedAck => {
                info!("Phone confirmed primary credentials: {}", ctx.name);
                None
            }

            Message::Takeover => self.handle_takeover(ctx),

            Message::Bye => {
                ctx.closing = true;
                // 手机自己说再见 = 这次临时访问结束：续连凭证作废
                if ctx.authed {
                    self.clear_transient_resume(&ctx.client_id);
                }
                info!("Bye from {} ({})", ctx.name, peer_ip);
                Some(Message::ByeAck)
            }

            _ => {
                warn!("Unexpected message type from {}", peer_ip);
                None
            }
        }
    }

    /// 该连接是否当前席位持有者（client_id + kick 指针双比对）
    fn is_session_holder(&self, ctx: &ConnCtx) -> bool {
        let session = self.session.lock().unwrap();
        session
            .as_ref()
            .is_some_and(|a| a.client_id == ctx.client_id && Arc::ptr_eq(&a.kick, &ctx.kick))
    }

    /// 握手认证主流程
    #[allow(clippy::too_many_arguments)]
    fn handle_hello(
        &self,
        code: Option<String>,
        token: Option<String>,
        resume: Option<String>,
        client_id: String,
        device_name: String,
        hardware_id: Option<String>,
        has_local_credential: Option<bool>,
        peer_ip: IpAddr,
        ctx: &mut ConnCtx,
    ) -> Option<Message> {
        // 本机身份：拒绝类应答都要带上 device_id（手机用来核对拒绝方）
        let (device_id, pc_name) = self.device_identity();

        // 记下硬件指纹：设为常用设备时写进记录（"重装后仍认得出"靠它）
        ctx.hardware_id = hardware_id.clone();

        let caps = Capabilities {
            auto_paste: self.auto_paste.load(Ordering::Relaxed),
            image: true,
        };
        let device_name = if device_name.trim().is_empty() {
            "手机".to_string()
        } else {
            device_name
        };

        // ---- 续连分支：临时会话断线后凭窗口期内的凭证免扫码续接 ----
        if let Some(resume_token) = resume {
            // 过期/不匹配都不计失败次数：这是正常路径（窗口过期就该重新扫码），
            // 不是攻击面——凭证是 64 位 hex 随机串，猜不出来。
            let Some(name) = self.verify_transient_resume(&client_id, &resume_token) else {
                info!("Transient resume rejected (expired or unknown) from {peer_ip}");
                return Some(Message::hello_ack_err(
                    "unauthorized",
                    "临时会话已过期，请重新扫码",
                ));
            };

            let mut session = self.session.lock().unwrap();
            if let Some(cur) = session.as_ref() {
                if cur.client_id == client_id {
                    // 席位被**同一台手机**的旧连接占着 = 它自己的僵尸连接。
                    // 手机换网络时 socket 死在本地、发不出 FIN，电脑只能等 15 秒心跳超时，
                    // 而手机往往几秒后就回来续连——若不在这里踢旧接新，就会回「占用中」，
                    // 手机按规则把窗口作废、把人赶去重新扫码（用户实测踩到过）。
                    // 与绑定手机的凭证分支判定一致（那边走 PrimaryArbitration::ReplaceSelf）。
                    info!("Transient resume replaces stale connection for {client_id}");
                    kick_session(cur, KICK_REPLACED);
                } else {
                    // 真被别的手机占着（例如常用设备正在用）：按既有规则拒绝，连接不保持
                    return Some(Message::hello_ack_err("occupied", "电脑使用中").with_device_id(&device_id));
                }
            }
            let (seat, kick, identity) =
                ActiveSession::new(client_id.clone(), name.clone(), false, ctx);
            *session = Some(seat);
            drop(session);

            ctx.admit(client_id, name.clone(), kick, identity);
            self.publish_state();
            auto_hide_after_pairing(&self.app_handle);
            let ttl_secs = self.resume_ttl().as_secs();
            info!("Transient session resumed: {} from {}", name, peer_ip);
            let (device_id, pc_name) = self.device_identity();
            return Some(Message::hello_ack_ok(
                "transient",
                &device_id,
                &pc_name,
                None,
                None, // 不换凭证：窗口期由 resume_expires_in 通知手机更新
                caps,
            )
            .with_resume_expires_in(ttl_secs));
        }

        // ---- token 分支：持久常用设备免配重连 ----
        if let Some(token) = token {
            let verified = {
                let mut config = self.config.lock().unwrap();
                config.verify_token(&client_id, &token, &device_name, &peer_ip.to_string())
            };
            if !verified {
                warn!("Token rejected from {}", peer_ip);
                return Some(Message::hello_ack_err(
                    "unauthorized",
                    "设备未授权，请在电脑上重新配对",
                )
                .with_device_id(&device_id));
            }
            let mut session = self.session.lock().unwrap();
            match arbitrate_primary(session.as_ref(), &client_id) {
                PrimaryArbitration::Admit => {
                    let (seat, kick, identity) = ActiveSession::new(
                        client_id.clone(),
                        device_name.clone(),
                        true,
                        ctx,
                    );
                    *session = Some(seat);
                    drop(session);
                    ctx.admit(client_id, device_name, kick, identity);
                    self.publish_state();
                    self.push_prefs(); // 刚连上：把行为开关同步给手机，两端显示一致
                    auto_hide_after_pairing(&self.app_handle);
                    info!("Primary connected via token from {}", peer_ip);
                    Some(Message::hello_ack_ok(
                        "primary",
                        &device_id,
                        &pc_name,
                        None,
                        None,
                        caps,
                    ))
                }
                PrimaryArbitration::ReplaceSelf => {
                    // 同一常用设备的旧连接还挂着（僵尸重连）：要求旧连接退出，新连接接席
                    if let Some(old) = session.as_ref() {
                        kick_session(old, KICK_REPLACED);
                    }
                    let (seat, kick, identity) = ActiveSession::new(
                        client_id.clone(),
                        device_name.clone(),
                        true,
                        ctx,
                    );
                    *session = Some(seat);
                    drop(session);
                    ctx.admit(client_id, device_name, kick, identity);
                    self.publish_state();
                    self.push_prefs(); // 刚连上：把行为开关同步给手机，两端显示一致
                    auto_hide_after_pairing(&self.app_handle);
                    info!("Primary replaced stale connection from {}", peer_ip);
                    Some(Message::hello_ack_ok(
                        "primary",
                        &device_id,
                        &pc_name,
                        None,
                        None,
                        caps,
                    ))
                }
                PrimaryArbitration::WaitTakeover => {
                    // 席位被临时方占用：连接保持，等待手动接管。
                    // 这里不设身份——席位还在别人手里，等接管成功由 admit 重新绑定
                    drop(session);
                    ctx.client_id = client_id;
                    ctx.name = device_name;
                    ctx.authed = true;
                    ctx.waiting_takeover = true;
                    ctx.last_active = Instant::now();
                    info!("Primary arrives but seat occupied from {}", peer_ip);
                    Some(Message::hello_ack_err("occupied", "电脑使用中").with_device_id(&device_id))
                }
            }
        } else if let Some(code) = code {
            // ---- 码分支：扫码 / 手动输码 ----
            let code_ok = {
                let config = self.config.lock().unwrap();
                config.verify_code(&code)
            };
            if !code_ok {
                warn!("Bad code from {}", peer_ip);
                return Some(Message::hello_ack_err("bad_code", "配对码错误或已过期").with_device_id(&device_id));
            }
            let mut session = self.session.lock().unwrap();
            if session.is_some() {
                // 码对但席位占用：不认证、不绑定，直接拒绝
                return Some(Message::hello_ack_err("occupied", "电脑使用中").with_device_id(&device_id));
            }

            // 席位空闲：判定身份。
            //  - 勾选"记住"且尚无常用设备：绑定为新常用设备，下发新 token
            //  - 来者 client_id 就是已绑定常用设备（常用设备手动断开后扫码回来）：
            //    直接恢复常用设备席位，token 不下发（手机端本就保存着）
            //  - 其余：临时设备
            // 无 client_id 的旧客户端不允许占常用设备席位
            let remember = self.remember_next.load(Ordering::Relaxed);
            let (is_primary, token_out, new_code, returning_primary, rebind) = {
                let mut config = self.config.lock().unwrap();
                // returning: client_id 匹配 OR hardware_id 匹配
                // 硬件指纹是关键——手机重装后 client_id 变了，但 hardware_id 不变，
                // 仍能被识别为"我认识这台手机"并恢复常用设备身份
                let client_match = !client_id.is_empty()
                    && config
                        .primary
                        .as_ref()
                        .is_some_and(|p| p.client_id == client_id);
                let hw_match = hardware_id.as_deref().is_some_and(|hid| {
                    config
                        .primary
                        .as_ref()
                        .is_some_and(|p| p.hardware_id.as_deref() == Some(hid))
                });
                let returning = client_match || hw_match;
                // 手机说它本地已经没有长期凭证了（重装 / 清了数据）：只"认出它"是不够的——
                // 不给它 token，它永远存不下东西，断线后只能靠临时窗口撑着（用户实测：
                // 重装后再扫码看着连上了，其实什么都没落盘，闲置几分钟就被赶回扫码页）。
                // 所以这种情况照常补发一个新 token（等于重新绑定一次，顺带刷新记录里的
                // 硬件指纹）。旧客户端不发这个字段（None）→ 按"有凭证"处理，行为不变。
                let rebind = returning
                    && has_local_credential == Some(false)
                    && !client_id.is_empty();
                let bind =
                    (remember && !config.has_primary() && !client_id.is_empty()) || rebind;
                let token_out = if bind {
                    Some(config.bind_primary(
                        &client_id,
                        &device_name,
                        &peer_ip.to_string(),
                        hardware_id.as_deref(),
                    ))
                } else {
                    None
                };
                // 配对码一次性：用过后立即换新，杜绝重放
                let new_code = config.regenerate_code();
                (bind || returning, token_out, new_code, returning, rebind)
            };
            self.set_code(new_code);

            // 临时会话：签发续连凭证（窗口期 0 = 关闭续连时不签发）。
            // 常用设备不需要——它本来就有长期 token。
            let resume_issue = if is_primary {
                None
            } else {
                self.issue_transient_resume(&client_id, &device_name)
            };

            let (device_id, pc_name) = self.device_identity();
            let (seat, kick, identity) =
                ActiveSession::new(client_id.clone(), device_name.clone(), is_primary, ctx);
            *session = Some(seat);
            drop(session);

            ctx.admit(client_id, device_name, kick, identity);
            self.publish_state();
            self.push_prefs(); // 刚配对成功：把行为开关同步给手机，两端显示一致
            auto_hide_after_pairing(&self.app_handle);
            info!(
                "Code pairing OK from {} ({})",
                peer_ip,
                if is_primary {
                    if rebind {
                        "rebind (phone had no local credential)"
                    } else if returning_primary {
                        "returning primary"
                    } else {
                        "primary"
                    }
                } else {
                    "transient"
                }
            );
            Some(Message::hello_ack_ok(
                if is_primary { "primary" } else { "transient" },
                &device_id,
                &pc_name,
                token_out,
                resume_issue,
                caps,
            ))
        } else {
            // 既无码也无 token
            warn!("Empty credentials from {}", peer_ip);
            Some(Message::hello_ack_err("unauthorized", "握手缺少凭证").with_device_id(&device_id))
        }
    }

    /// 把当前在线席位上的**临时**手机设为常用设备（设置页「常用设备」的按钮）。
    ///
    /// 只在"还没有常用设备 + 当前席位是临时身份"时允许。凭证由 PC 生成后推给手机，
    /// 手机落盘即成为常用设备——效果等价于扫码时勾选"记住这台手机"。
    /// 推送失败会回滚绑定，避免"PC 以为绑了、手机没凭据"的不一致。
    pub fn promote_current_session(&self) -> Result<(), String> {
        let (client_id, name, writer, hardware_id, ip) = {
            let session = self.session.lock().unwrap();
            let a = session.as_ref().ok_or("当前没有手机连接")?;
            if a.identity.load(Ordering::SeqCst) {
                return Err("这台手机已经绑定过了".to_string());
            }
            (
                a.client_id.clone(),
                a.name.clone(),
                a.writer.clone(),
                a.hardware_id.clone(),
                a.ip.clone(),
            )
        };

        let token = {
            let mut config = self.config.lock().unwrap();
            if config.has_primary() {
                return Err("已经绑定了手机，请先解除绑定".to_string());
            }
            config.bind_primary(&client_id, &name, &ip, hardware_id.as_deref())
        };

        let (device_id, pc_name) = self.device_identity();
        let caps = Capabilities {
            auto_paste: self.auto_paste.load(Ordering::Relaxed),
            image: true,
        };
        let delivered = writer
            .as_ref()
            .is_some_and(|w| send(w, &Message::promoted(&token, &device_id, &pc_name, caps)));
        if !delivered {
            self.config.lock().unwrap().unbind_primary();
            return Err("下发凭证失败，连接可能已断开".to_string());
        }

        // 席位身份升级为常用设备；临时续连记录作废（手机接下来用长期 token）
        {
            let session = self.session.lock().unwrap();
            if let Some(a) = session.as_ref() {
                if a.client_id == client_id {
                    a.identity.store(true, Ordering::SeqCst);
                }
            }
        }
        self.clear_transient_resume(&client_id);
        self.publish_state();
        info!("Session promoted to primary: {name} ({client_id})");
        Ok(())
    }

    /// 重启应用：延迟启动一份新进程，然后退出当前进程。
    ///
    /// 为什么"延迟"：单实例插件在检测到已有实例时会直接 exit(0)。若新旧进程重叠，
    /// 新进程会认为自己不该启动而自杀、旧进程同时也在退出 —— 应用凭空消失。
    /// 因此先起一个"等 1 秒再 exec"的启动器，确认 spawn 成功后才退出自己。
    ///
    /// 路径来自启动时捕获的 exe_path，所以"先装新 deb、再点重启"会启动到**新装**的
    /// 二进制（运行时读 /proc/self/exe 会拿到 "(deleted)" 路径而失败）。
    pub fn restart_self(&self) -> Result<String, String> {
        let exe = self
            .exe_path
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "无法确定程序路径".to_string())?;
        spawn_relaunch(&exe)?;
        Ok(exe.display().to_string())
    }

    fn device_identity(&self) -> (String, String) {
        let config = self.config.lock().unwrap();
        (config.device_id.clone(), config.device_name.clone())
    }

    /// 常用设备点击"接管电脑"
    fn handle_takeover(&self, ctx: &mut ConnCtx) -> Option<Message> {
        if !ctx.authed || !ctx.waiting_takeover {
            warn!("Takeover rejected: connection not eligible");
            return Some(Message::TakeoverAck { ok: false });
        }

        let mut session = self.session.lock().unwrap();
        // 通知当前持有者结束（若期间刚好已空出，则无人可踢）
        if let Some(cur) = session.as_ref() {
            kick_session(cur, KICK_TAKEOVER);
        }
        let (seat, kick, identity) =
            ActiveSession::new(ctx.client_id.clone(), ctx.name.clone(), true, ctx);
        *session = Some(seat);
        drop(session);

        ctx.waiting_takeover = false;
        ctx.kick = kick;
        ctx.identity = identity;
        ctx.last_active = Instant::now();
        self.publish_state();
        auto_hide_after_pairing(&self.app_handle);
        info!("Primary took over the seat: {}", ctx.client_id);
        Some(Message::TakeoverAck { ok: true })
    }

    /// 文本入剪贴板 + 自动粘贴（认证通过后调用）
    /// 图片消息的准入判定：与文本一致，必须是席位持有者（等待接管 / 未认证一律拒）
    fn image_guard(&self, ctx: &ConnCtx, peer_ip: IpAddr, id: &str) -> Option<Message> {
        let is_holder = self.is_session_holder(ctx);
        if !ctx.authed || ctx.waiting_takeover || !is_holder {
            warn!("Image message from non-session connection {peer_ip}");
            return Some(self.image_ack_fail(id.to_string(), "not_connected"));
        }
        // 临时设备仍在活跃：续连窗口往后推（与文本同一条待遇）
        if !ctx.identity.load(Ordering::SeqCst) {
            self.touch_transient_resume(&ctx.client_id);
        }
        None
    }

    fn image_ack_fail(&self, id: String, code: &str) -> Message {
        Message::Ack {
            id,
            ok: false,
            code: Some(code.to_string()),
            pasted: false,
            srv_recv_ms: now_ms(),
        }
    }

    /// 收到完整图片：解码 → 写系统剪贴板 → 自动粘贴。
    ///
    /// **刻意不自动回车**：贴完图通常还要配文字，自动回车会立刻把消息发出去。
    /// 文本那条链路（`handle_text`）的可选回车不适用于图片。
    fn handle_image(&self, id: String, mime: String, bytes: Vec<u8>) -> Option<Message> {
        let recv_ms = now_ms();
        let img = match image::load_from_memory(&bytes) {
            Ok(i) => i,
            Err(e) => {
                warn!("Image decode failed ({mime}, {} bytes): {e}", bytes.len());
                return Some(self.image_ack_fail(id, "image_decode_error"));
            }
        };
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 || (w as u64) * (h as u64) > crate::image_receiver::MAX_DECODED_PIXELS {
            warn!("Image dimensions rejected: {w}x{h}");
            return Some(self.image_ack_fail(id, "image_too_large"));
        }
        let rgba = img.to_rgba8();
        let (rw, rh) = rgba.dimensions();
        let image = tauri::image::Image::new_owned(rgba.into_raw(), rw, rh);
        if let Err(e) = self.app_handle.clipboard().write_image(&image) {
            warn!("Clipboard image write failed: {e}");
            return Some(self.image_ack_fail(id, "clipboard_error"));
        }

        // 自动粘贴：与文本同样的三个条件（开关开、平台支持、我们自己的窗口没有焦点）。
        // 这里不再走 will_paste 那套剪贴板接管逻辑——图片路径只注入 Ctrl+V。
        let enabled = self.auto_paste.load(Ordering::Relaxed);
        let own_window_focused = self
            .app_handle
            .get_webview_window("main")
            .map(|w| w.is_focused().unwrap_or(false))
            .unwrap_or(false);
        let mut pasted = false;
        if enabled && crate::paste::auto_paste_supported() && !own_window_focused {
            match crate::paste::simulate_paste() {
                Ok(_) => {
                    pasted = true;
                    // 注入成功不等于目标读到了：剪贴板只有一个槽位，而手机收到 ack
                    // 就会发下一张 ⇒ 必须等目标把这一张读走再回 ack，否则下一张的
                    // 写入会覆盖它（"选 3 张只到 2 张"的根因，见 paste.rs 常量注释）
                    crate::paste::wait_image_settle(bytes.len());
                }
                Err(e) => warn!("Image auto-paste failed: {e}"),
            }
        }
        info!(
            "Image received: {w}x{h} ({} bytes, {mime}) pasted={pasted}",
            bytes.len()
        );
        Some(Message::Ack {
            id,
            ok: true,
            code: None,
            pasted,
            srv_recv_ms: recv_ms,
        })
    }

    fn handle_text(&self, id: String, text: String) -> Option<Message> {
        // 空文本直接拒绝，避免覆盖剪贴板
        if text.trim().is_empty() {
            info!("Ignored empty text (id={})", id);
            return Some(Message::Ack {
                id,
                ok: false,
                code: Some("empty_text".to_string()),
                pasted: false,
                srv_recv_ms: now_ms(),
            });
        }

        let recv_ms = now_ms();
        // 写入 + 读回校验：把"另一个进程夺走剪贴板""写入被静默丢弃"这类
        // 机器差异变成可观测的结果，而不是假定成功（见 clipboard.rs）
        if !crate::clipboard::write_verified(&self.app_handle, &text) {
            let _ = self
                .app_handle
                .notification()
                .builder()
                .title("BeepType")
                .body("剪贴板写入失败，请重试")
                .show();
            return Some(Message::Ack {
                id,
                ok: false,
                code: Some("clipboard_error".to_string()),
                pasted: false,
                srv_recv_ms: recv_ms,
            });
        }

        // 自动粘贴到光标处：
        // 1. 设置页开关已开启；
        // 2. 当前平台能模拟按键（X11/Windows/macOS，以及 wlroots 系 Wayland；
        //    GNOME/KDE 的 Wayland 回退纯剪贴板）；
        // 3. BeepType 自己的窗口没有焦点，避免文字糊进配对/状态窗口。
        let mut pasted = false;
        let enabled = self.auto_paste.load(Ordering::Relaxed);
        let own_window_focused = self
            .app_handle
            .get_webview_window("main")
            .map(|w| w.is_focused().unwrap_or(false))
            .unwrap_or(false);
        if enabled && crate::paste::auto_paste_supported() && !own_window_focused {
            // 注入前记下焦点窗口：回车前复核它没变，否则回车会落到用户刚切过去的
            // 窗口里（这条检查是"给足回车余量"的前提，见 docs/TECH_DESIGN.md 第 10 节）
            let focus_before = crate::paste::focused_window_key();
            match crate::paste::simulate_paste() {
                Ok(enigo) => {
                    pasted = true;
                    // 自动发送：粘贴成功后模拟 Enter，复用同一条 enigo 连接
                    let auto_send = self.auto_send.load(Ordering::Relaxed);
                    if auto_send && crate::paste::auto_send_supported() {
                        // 顺序：先等满回车余量 → 再复核焦点 → 最后才按下回车。
                        // 复核必须发生在等待之后，覆盖的才是"粘贴完成到回车落下"
                        // 这段几百毫秒（用户最可能在这里切窗口）。
                        crate::paste::wait_before_enter(text.chars().count());
                        let focus_after = crate::paste::focused_window_key();
                        log::debug!("Focus check before/after enter: {focus_before:?} / {focus_after:?}");
                        if crate::paste::should_skip_enter(focus_before, focus_after) {
                            // 用户在这几百毫秒里切了窗口：不发回车（文字已粘贴，
                            // 需要的话由用户自己按回车），只记日志不打扰
                            info!("Auto-send skipped: focus window changed during paste");
                        } else if crate::paste::focus_is_terminal() {
                            // 终端里 Enter 是"执行命令"而不是"发送"，这条损失真实且不可逆
                            // ⇒ 一律不自动按（要开的话等「自定义键位」里的发送键设置）
                            info!("Auto-send skipped: focused window is a terminal");
                        } else if let Some(mut enigo) = enigo {
                            match crate::paste::send_enter(&mut enigo) {
                                Ok(()) => info!("Auto-send: Enter sent"),
                                Err(e) => warn!("Auto-send failed: {}", e),
                            }
                        } else {
                            // 这次不是 enigo 发的（Wayland 终端走 wtype）：没有可复用的
                            // 连接，而且终端本来就不自动回车，正常不该走到这里
                            info!("Auto-send skipped: no reusable key connection (wtype path)");
                        }
                    }
                }
                Err(e) => warn!("Auto-paste failed (text stays in clipboard): {}", e),
            }
        } else if enabled && own_window_focused {
            info!("Skip auto-paste: BeepType window is focused");
        }

        Some(Message::Ack {
            id,
            ok: true,
            code: None,
            pasted,
            srv_recv_ms: recv_ms,
        })
    }
}

/// 配对成功后的窗口收回（受设置项「连接后自动隐藏窗口」控制，默认开启）：
/// 先通知前端立刻渲染绿色"已连接"页，停留约 0.9 秒作为确认，再自动隐藏到托盘，
/// 用户无需手动点叉。断线自动重连成功时同样触发（窗口本就隐藏时 hide 为空操作）。
///
/// 关掉该设置后窗口一直留在桌面（便于盯着状态、管理设备）。
fn auto_hide_after_pairing<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<Arc<Mutex<ServerState<R>>>>();
    let server = state.lock().unwrap();
    let _ = app.emit("pairing-show", ());
    if !server.auto_hide.load(Ordering::Relaxed) {
        return;
    }
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(900));
        // 隐藏前再查一次：用户可能在 0.9s 内把设置关掉了
        let state = app.state::<Arc<Mutex<ServerState<R>>>>();
        let enabled = state
            .lock()
            .unwrap()
            .auto_hide
            .load(Ordering::Relaxed);
        if enabled {
            // 销毁而不是隐藏：把 WebKit 进程还给系统，下次唤起再重建
            // （先关 webview 再销毁窗口，顺序不能反——见 lib.rs 的 close_main_window）
            crate::close_main_window(&app);
        }
    });
}

/// 启动 TCP 服务端（在后台线程运行）
pub fn serve<R: Runtime>(state: ServerState<R>) {
    let port = state.port;
    let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)) {
        Ok(l) => {
            info!("Listening on 0.0.0.0:{}", port);
            l
        }
        Err(e) => {
            log::error!("Failed to bind port {}: {}", port, e);
            return;
        }
    };

    for stream_result in listener.incoming() {
        match stream_result {
            Ok(stream) => {
                let state = state.clone();
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(e) => {
                warn!("Accept failed: {}", e);
            }
        }
    }
}

/// 处理单个客户端连接
fn handle_connection<R: Runtime>(stream: TcpStream, state: ServerState<R>) {
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip());

    // TCP keepalive
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let idle: i32 = 10;
        let interval: i32 = 5;
        let count: i32 = 3;
        // Linux 用 TCP_KEEPIDLE，macOS/BSD 用 TCP_KEEPALIVE
        #[cfg(target_os = "linux")]
        let idle_opt = libc::TCP_KEEPIDLE;
        #[cfg(target_os = "macos")]
        let idle_opt = libc::TCP_KEEPALIVE;
        unsafe {
            libc::setsockopt(fd, libc::IPPROTO_TCP, idle_opt, &idle as *const _ as *const libc::c_void, std::mem::size_of::<i32>() as u32);
            libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, &interval as *const _ as *const libc::c_void, std::mem::size_of::<i32>() as u32);
            libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, &count as *const _ as *const libc::c_void, std::mem::size_of::<i32>() as u32);
        }
    }

    let stream = match stream.set_read_timeout(Some(std::time::Duration::from_secs(READ_TIMEOUT_SECS))) {
        Ok(_) => stream,
        Err(e) => {
            warn!("set_read_timeout failed: {}", e);
            return;
        }
    };

    let peer = peer_ip.unwrap_or_else(|| "unknown".parse().unwrap());
    info!("Connection from {}", peer);

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut ctx = ConnCtx::default();
    ctx.wake = stream.try_clone().ok();
    ctx.peer_ip = Some(peer);
    let writer: SharedWriter = Arc::new(Mutex::new(stream));
    ctx.writer = Some(writer.clone());

    let mut line = String::new();
    'conn: loop {
        // 被踢/被替换：发 session_ended 后退出（清理段靠 kick 指针比对不会误清新席位）
        let kicked = ctx.kick.load(Ordering::SeqCst);
        if kicked != 0 {
            send_session_ended(&writer, &ctx.name, kicked);
            break 'conn;
        }

        // 身份变化（解绑降级 / 设为常用）**不在这里发消息**：由命令线程当场通过
        // 共享写句柄推送（见 unbind_primary / promote_current_session）。
        // 本线程只负责"读"——续连窗口判定与心跳里的身份回报都直接读 ctx.identity。

        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // shutdown(Read) 唤醒的踢人路径也表现为 EOF：先查 kick 再下结论
                let kicked = ctx.kick.load(Ordering::SeqCst);
                if kicked != 0 {
                    send_session_ended(&writer, &ctx.name, kicked);
                    break 'conn;
                }
                info!("Connection closed by {}", peer);
                break 'conn;
            }
            Ok(n) => {
                if n > MAX_LINE_BYTES {
                    warn!("Message too large: {} bytes from {}", n, peer);
                    break 'conn;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if let Some(reply) = state.handle_message(trimmed, peer, &mut ctx) {
                    if !send(&writer, &reply) {
                        break 'conn;
                    }
                }
                if ctx.closing {
                    break 'conn;
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
                    // 已认证连接：连接级心跳 15 秒兜底
                    if ctx.authed && ctx.last_active.elapsed().as_secs() > HEARTBEAT_TIMEOUT_SECS {
                        warn!("Heartbeat timeout from {}", peer);
                        break 'conn;
                    }
                    // 图片收了一半没下文：超时丢弃，并告知手机端（否则手机端只能干等）
                    if let Some(crate::image_receiver::ImageProgress::Failed { id, code, message }) =
                        ctx.image.tick()
                    {
                        warn!("image receive timeout: {message}");
                        let reply = Message::Ack {
                            id,
                            ok: false,
                            code: Some(code.to_string()),
                            pasted: false,
                            srv_recv_ms: now_ms(),
                        };
                        if !send(&writer, &reply) {
                            break 'conn;
                        }
                    }
                // 未认证连接空转等待，由 TCP keepalive 与对端关闭负责
                } else {
                    warn!("Read error from {}: {}", peer, e);
                    break 'conn;
                }
            }
        }
    }

    // 连接退出：仅当自己仍是席位持有者时释放（被踢/被替换时席位已属他人）
    if ctx.authed && !ctx.waiting_takeover {
        let mut session = state.session.lock().unwrap();
        let should_release = session
            .as_ref()
            .is_some_and(|a| a.client_id == ctx.client_id && Arc::ptr_eq(&a.kick, &ctx.kick));
        if should_release {
            *session = None;
            drop(session);
            state.publish_state();
            info!("Seat released by disconnect: {} ({})", ctx.name, peer);
        }
    }
}

/// 重启前等待旧进程退出的秒数（给单实例锁释放留出余量）
const RESTART_DELAY_SECS: u64 = 1;

/// 构造"延迟重启"命令（程序名, 参数）。独立成函数是为了可单测：
/// 路径必须作为参数传入，不能拼进脚本文本里（否则路径含引号/空格就会坏）。
fn relaunch_command(exe: &Path) -> (String, Vec<std::ffi::OsString>) {
    #[cfg(unix)]
    {
        (
            "sh".to_string(),
            vec![
                "-c".into(),
                format!("sleep {RESTART_DELAY_SECS}; exec \"$0\"").into(),
                exe.as_os_str().to_owned(),
            ],
        )
    }
    #[cfg(windows)]
    {
        // Windows 没有 sh：路径通过环境变量交给 cmd，同样避免引号拼接
        (
            "cmd".to_string(),
            vec![
                "/C".into(),
                format!(
                    "timeout /t {RESTART_DELAY_SECS} /nobreak >nul & start \"\" \"%BEEPTYPE_RESTART_EXE%\""
                )
                .into(),
            ],
        )
    }
}

/// 拉起延迟重启进程（失败返回错误，调用方据此决定是否还要退出自己）
fn spawn_relaunch(exe: &Path) -> Result<(), String> {
    let (program, args) = relaunch_command(exe);
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.env("BEEPTYPE_RESTART_EXE", exe);
    #[cfg(unix)]
    unsafe {
        // 脱离当前会话，避免父进程（或终端）退出时把它一起带走
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("启动新进程失败：{e}"))
}

/// 获取局域网 IP（优先 IPv4）。
///
/// 结果缓存 5 秒：窗口可见时前端每秒取一次配对信息，而每次都要新建 socket、
/// 做一次路由选择。不能永久缓存——Wi-Fi↔有线切换后二维码里的地址必须自己纠正。
pub fn lan_ip() -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const TTL: Duration = Duration::from_secs(5);
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<String>)>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((at, ip)) = cache.as_ref() {
        if at.elapsed() < TTL {
            return ip.clone();
        }
    }
    let fresh = detect_lan_ip();
    *cache = Some((Instant::now(), fresh.clone()));
    fresh
}

fn detect_lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// 生成 QR 码内容字符串
/// 格式：beeptype://<ip>:<port>?code=<code>
pub fn qr_content(ip: &str, port: u16, code: &str) -> String {
    format!("beeptype://{}:{}?code={}", ip, port, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_content() {
        let qr = qr_content("192.168.1.100", 8765, "123456");
        assert_eq!(qr, "beeptype://192.168.1.100:8765?code=123456");
    }

    #[test]
    fn test_arbitrate_primary() {
        let kick = Arc::new(AtomicU8::new(0));
        let primary_session = ActiveSession {
            client_id: "p1".to_string(),
            name: "常用设备".to_string(),
            identity: Arc::new(AtomicBool::new(true)),
            kick: kick.clone(),
            wake: Mutex::new(None),
            writer: None,
            hardware_id: None,
            ip: String::new(),
        };
        let transient_session = ActiveSession {
            client_id: "t1".to_string(),
            name: "临时设备".to_string(),
            identity: Arc::new(AtomicBool::new(false)),
            kick: Arc::new(AtomicU8::new(0)),
            wake: Mutex::new(None),
            writer: None,
            hardware_id: None,
            ip: String::new(),
        };

        // 空席：直接接入
        assert_eq!(arbitrate_primary(None, "p1"), PrimaryArbitration::Admit);
        // 同一常用设备旧连接：踢旧接新
        assert_eq!(
            arbitrate_primary(Some(&primary_session), "p1"),
            PrimaryArbitration::ReplaceSelf
        );
        // 临时方占用：等待接管
        assert_eq!(
            arbitrate_primary(Some(&transient_session), "p1"),
            PrimaryArbitration::WaitTakeover
        );
        // 另一 client_id 面对常用设备占用（理论上一台 PC 只有一个常用设备，逻辑仍应等待）
        assert_eq!(
            arbitrate_primary(Some(&primary_session), "p2"),
            PrimaryArbitration::WaitTakeover
        );
        assert_eq!(kick.load(Ordering::SeqCst), 0); // 纯仲裁函数不产生副作用
    }

    /// 临时续连记录：哈希匹配 + client_id 匹配 + 未过期，三者缺一不可
    #[test]
    fn test_transient_resume_verify() {
        let token = "a".repeat(64);
        let now = Instant::now();
        let record = TransientResume {
            client_id: "client-1".to_string(),
            token_hash: crate::config::sha256_hex(&token),
            name: "临时手机".to_string(),
            expires_at: now + std::time::Duration::from_secs(300),
        };

        assert!(record.verify("client-1", &token, now));
        assert!(!record.verify("client-2", &token, now), "client_id 不符应拒绝");
        assert!(!record.verify("client-1", "deadbeef", now), "token 不符应拒绝");
        // 免得出错：明文 token 本身不能当哈希用
        assert!(!record.verify("client-1", &record.token_hash, now));
    }

    /// 窗口期边界：到期时刻起即失效
    #[test]
    fn test_transient_resume_expiry() {
        let token = "b".repeat(64);
        let now = Instant::now();
        let record = TransientResume {
            client_id: "c".to_string(),
            token_hash: crate::config::sha256_hex(&token),
            name: "手机".to_string(),
            expires_at: now + std::time::Duration::from_secs(5),
        };
        assert!(!record.is_expired(now));
        assert!(!record.is_expired(now + std::time::Duration::from_secs(4)));
        assert!(record.is_expired(now + std::time::Duration::from_secs(5)));
        assert!(!record.verify("c", &token, now + std::time::Duration::from_secs(6)));
    }

    /// 重启命令：必须"延迟启动"（等旧进程退出，否则新进程会被单实例锁挡掉而自杀），
    /// 且可执行文件路径作为**独立参数**传入、不拼进脚本字符串（路径含空格/引号也安全）
    #[test]
    fn test_relaunch_command_shape() {
        let exe = Path::new("/usr/bin/beeptype");
        let (program, args) = relaunch_command(exe);

        #[cfg(unix)]
        {
            assert_eq!(program, "sh");
            assert_eq!(args.len(), 3);
            assert_eq!(args[0].to_string_lossy(), "-c");
            let script = args[1].to_string_lossy();
            assert!(script.starts_with("sleep "), "应先等旧进程退出再启动");
            assert!(script.contains("$0"), "路径应通过参数传入");
            assert!(
                !script.contains("/usr/bin/beeptype"),
                "路径不能拼进脚本文本"
            );
            assert_eq!(args[2].to_string_lossy(), "/usr/bin/beeptype");
        }
        #[cfg(windows)]
        {
            assert_eq!(program, "cmd");
            let script = args.last().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
            assert!(script.contains("timeout /t"), "应先等旧进程退出再启动");
            assert!(script.contains("%BEEPTYPE_RESTART_EXE%"), "路径应经环境变量传入");
        }
    }

    #[test]
    fn test_kick_reason_mapping() {
        assert_eq!(kick_reason(KICK_TAKEOVER), "owner_takeover");
        assert_eq!(kick_reason(KICK_DISCONNECT), "owner_disconnect");
        assert_eq!(kick_reason(KICK_REPLACED), "replaced");
    }
}


/// 集成测试：用**真实 TCP 连接**扮演手机，验证握手/席位/凭证这套状态机。
///
/// 之所以能做到无头运行：`ServerState` 对运行时泛型化后，测试里可以用 tauri 的
/// `mock_app()`（MockRuntime）起一个实例。这里覆盖的正是"某条路径忘了做某事"
/// 这类单测挡不住的漏洞——例如曾经的"解绑降级后没有补发续连凭证"。
#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    type TestState = ServerState<tauri::test::MockRuntime>;

    /// 读一行并解析成 JSON
    fn read_json(reader: &mut BufReader<TcpStream>) -> serde_json::Value {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read_line");
        assert!(n > 0, "连接被对端关闭");
        serde_json::from_str(line.trim()).expect("应当是一行合法 JSON")
    }

    /// 读到指定 type 的消息为止（心跳等可能交错）
    fn read_until(reader: &mut BufReader<TcpStream>, kind: &str) -> serde_json::Value {
        for _ in 0..8 {
            let v = read_json(reader);
            if v["type"] == kind {
                return v;
            }
        }
        panic!("没有等到 {kind} 消息");
    }

    /// 建一个测试用服务端：配对码已知（regenerate_code 返回明文）
    fn test_state() -> (TestState, String) {
        let app = tauri::test::mock_app();
        let mut config = crate::config::Config::default();
        config.device_name = "测试电脑".to_string();
        // 真实启动流程会补一个 UUID（Config::load 里），测试自己造一个，
        // 否则"拒绝应答要带 device_id"这类断言无从核对
        config.device_id = "test-pc-id".to_string();
        let code = config.regenerate_code();
        let state = ServerState::new(
            app.handle().clone(),
            8765,
            code.clone(),
            false, // auto_paste
            false, // auto_send
            true,  // auto_hide
            config,
        );
        // 配对成功/降级的回调里会通过 app.state() 取它
        app.manage(Arc::new(Mutex::new(state.clone())));
        (state, code)
    }

    /// 起一个只接受一条连接的"服务端"，返回客户端要连的地址
    fn spawn_server(state: TestState) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handle_connection(stream, state);
            }
        });
        addr
    }

    /// 扮演手机连上去
    fn connect_as_phone(addr: SocketAddr) -> (TcpStream, BufReader<TcpStream>) {
        connect_as_phone_timeout(addr, std::time::Duration::from_secs(5))
    }

    /// 同上，但指定读超时：用来断言"消息必须在 N 秒内到"
    fn connect_as_phone_timeout(
        addr: SocketAddr,
        timeout: std::time::Duration,
    ) -> (TcpStream, BufReader<TcpStream>) {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(timeout)).expect("set_read_timeout");
        let reader = BufReader::new(sock.try_clone().expect("clone"));
        (sock, reader)
    }

    fn send_hello(sock: &mut TcpStream, hello: serde_json::Value) {
        sock.write_all(format!("{hello}\n").as_bytes()).expect("write hello");
    }

    /// 主人解绑 → 连接降级为临时，**必须补发续连凭证**，且该凭证能真的用
    /// （这条正是用户实测发现的 bug：降级来的临时会话被系统杀进程后回不来）
    #[test]
    fn test_downgrade_issues_resume_and_grant_works() {
        let (state, code) = test_state();
        state.set_remember_next(true); // 这次码配对绑定为常用设备
        // 只等 1 秒：降级通知必须**当场**发出，不许等手机下一条心跳
        // （历史上靠连接线程轮询的实现在这里必然超时——那正是"手机上标签慢半拍"的根因）
        let (mut sock, mut reader) = connect_as_phone_timeout(
            spawn_server(state.clone()),
            std::time::Duration::from_secs(1),
        );

        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-1","device_name":"测试手机"}),
        );
        let ack = read_until(&mut reader, "hello_ack");
        assert_eq!(ack["ok"], true);
        assert_eq!(ack["session"], "primary");
        assert!(ack["token"].is_string(), "绑定常用设备应下发 token");
        assert!(ack["resume_token"].is_null(), "常用设备不需要续连凭证");

        // 主人解绑 → 降级通知当场到达，手机全程一句话都没说
        state.unbind_primary();
        let ev = read_until(&mut reader, "session_downgraded");
        let resume_token = ev["resume_token"].as_str().unwrap_or_default().to_string();
        assert!(
            !resume_token.is_empty(),
            "降级为临时必须补发续连凭证，否则杀进程后回不来"
        );
        assert!(ev["resume_expires_in"].as_u64().unwrap_or(0) > 0, "应带窗口时长");

        // 断开旧连接（释放席位），用补发的凭证续连
        drop(sock);
        drop(reader);
        std::thread::sleep(std::time::Duration::from_millis(300));

        let (mut sock2, mut reader2) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock2,
            serde_json::json!({"type":"hello","resume":resume_token,
                               "client_id":"cid-1","device_name":"测试手机"}),
        );
        let ack2 = read_until(&mut reader2, "hello_ack");
        assert_eq!(ack2["ok"], true, "凭补发的凭证应当能续连");
        assert_eq!(ack2["session"], "transient");
        assert!(
            ack2["resume_expires_in"].as_u64().unwrap_or(0) > 0,
            "续连成功应回传窗口时长"
        );
    }

    /// 扫码来的临时设备同样拿到凭证，并可用它续连（原本就正常，留作回归护栏）
    #[test]
    fn test_code_transient_gets_grant_and_resume_works() {
        let (state, code) = test_state();
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));

        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-t","device_name":"临时手机"}),
        );
        let ack = read_until(&mut reader, "hello_ack");
        assert_eq!(ack["session"], "transient", "未勾选记住 → 临时身份");
        let token = ack["resume_token"].as_str().unwrap_or_default().to_string();
        assert!(!token.is_empty(), "临时会话应拿到续连凭证");

        drop(sock);
        drop(reader);
        std::thread::sleep(std::time::Duration::from_millis(300));

        let (mut sock2, mut reader2) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock2,
            serde_json::json!({"type":"hello","resume":token,
                               "client_id":"cid-t","device_name":"临时手机"}),
        );
        let ack2 = read_until(&mut reader2, "hello_ack");
        assert_eq!(ack2["ok"], true);
        assert_eq!(ack2["session"], "transient");
    }

    /// 续连窗口被关闭（0 分钟）时不签发凭证，降级消息也不带
    #[test]
    fn test_no_grant_when_resume_disabled() {
        let (state, code) = test_state();
        state.config.lock().unwrap().resume_ttl_min = 0;
        state.set_remember_next(true);
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));

        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-2","device_name":"测试手机"}),
        );
        assert_eq!(read_until(&mut reader, "hello_ack")["session"], "primary");

        state.unbind_primary();
        let ev = read_until(&mut reader, "session_downgraded");
        assert!(ev["resume_token"].is_null(), "关闭续连时不应签发凭证");
    }

    /// 转正：把在线的临时手机设为常用设备 → 手机收到带长期 token 的 promoted，
    /// 该 token 之后能换到 primary 席位（等价于扫码时勾选"记住这台手机"）
    #[test]
    fn test_promote_transient_to_primary() {
        let (state, code) = test_state();
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));

        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-p","device_name":"临时手机"}),
        );
        let ack = read_until(&mut reader, "hello_ack");
        assert_eq!(ack["session"], "transient");
        assert!(!state.config.lock().unwrap().has_primary(), "设为常用前不应有绑定");

        // 主人在设置页点「设为常用设备」
        state.promote_current_session().expect("转正应当成功");
        assert!(state.config.lock().unwrap().has_primary(), "应写入常用设备记录");

        let ev = read_until(&mut reader, "promoted");
        let token = ev["token"].as_str().unwrap_or_default().to_string();
        assert_eq!(token.len(), 64, "长期 token 应为 64 位 hex");
        assert_eq!(ev["device_name"], "测试电脑");
        sock.write_all(b"{\"type\":\"promoted_ack\"}\n").expect("ack");

        // 再次设为常用应被拒（当前已是常用设备）
        assert!(
            state.promote_current_session().is_err(),
            "已经是常用设备时不应重复设置"
        );

        // 手机端拿到 token 后重连 → 应直接进 primary 席位
        drop(sock);
        drop(reader);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (mut sock2, mut reader2) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock2,
            serde_json::json!({"type":"hello","token":token,
                               "client_id":"cid-p","device_name":"临时手机"}),
        );
        let ack2 = read_until(&mut reader2, "hello_ack");
        assert_eq!(ack2["ok"], true);
        assert_eq!(ack2["session"], "primary", "设为常用后凭 token 应拿到常用设备席位");
    }

    /// 手机重装/清过数据后（本地已经没有长期凭证）扫码回来：必须**补发一个新 token**。
    ///
    /// 回归护栏：原行为是"认出老手机（client_id 或硬件指纹匹配）就直接恢复席位、
    /// 不下发 token"，前提是"手机本来就存着凭证"。可重装之后它手里什么都没有，
    /// 恢复席位也存不下东西——用户实测：扫码看着连上了，闲置几分钟又被赶回扫码页。
    #[test]
    fn test_rebind_when_phone_has_no_local_credential() {
        let (state, code) = test_state();
        state.set_remember_next(true); // 首次配对：勾了「记住这台手机」
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-old",
                               "device_name":"小米14","hardware_id":"hw-1"}),
        );
        let ack = read_until(&mut reader, "hello_ack");
        assert_eq!(ack["session"], "primary");
        let first_token = ack["token"].as_str().unwrap_or_default().to_string();
        assert_eq!(first_token.len(), 64, "首绑应下发 64 位 token");
        drop(sock);
        drop(reader);
        std::thread::sleep(std::time::Duration::from_millis(300));

        // 模拟重装：新的 client_id、**同一个硬件指纹**，并声明"我本地没有长期凭证"
        let code2 = state.config.lock().unwrap().regenerate_code();
        state.set_code(code2.clone());
        state.set_remember_next(false); // 重装后扫码，用户没勾「记住」
        let (mut sock2, mut reader2) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock2,
            serde_json::json!({"type":"hello","code":code2,"client_id":"cid-new",
                               "device_name":"小米14","hardware_id":"hw-1",
                               "has_local_credential":false}),
        );
        let ack2 = read_until(&mut reader2, "hello_ack");
        assert_eq!(ack2["session"], "primary", "硬件指纹匹配 → 仍认作常用设备");
        let token2 = ack2["token"].as_str().unwrap_or_default().to_string();
        assert_eq!(
            token2.len(),
            64,
            "手机本地没有凭证时必须补发新 token，否则它什么都存不下"
        );
        assert_ne!(token2, first_token, "补发的是一把新凭证");
        let cfg = state.config.lock().unwrap();
        let p = cfg.primary.as_ref().expect("应写入常用设备记录");
        assert_eq!(p.client_id, "cid-new", "绑定记录应跟着更新为新手机");
        assert_eq!(p.hardware_id.as_deref(), Some("hw-1"));
    }

    /// 反例：手机说自己**有**本地凭证时，不下发 token（保持原行为，不白换凭证）。
    #[test]
    fn test_returning_with_credential_gets_no_new_token() {
        let (state, code) = test_state();
        state.set_remember_next(true);
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-old",
                               "device_name":"小米14","hardware_id":"hw-1"}),
        );
        assert_eq!(read_until(&mut reader, "hello_ack")["session"], "primary");
        drop(sock);
        drop(reader);
        std::thread::sleep(std::time::Duration::from_millis(300));

        let code2 = state.config.lock().unwrap().regenerate_code();
        state.set_code(code2.clone());
        state.set_remember_next(false);
        let (mut sock2, mut reader2) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock2,
            serde_json::json!({"type":"hello","code":code2,"client_id":"cid-old",
                               "device_name":"小米14","hardware_id":"hw-1",
                               "has_local_credential":true}),
        );
        let ack2 = read_until(&mut reader2, "hello_ack");
        assert_eq!(ack2["session"], "primary");
        assert!(
            ack2["token"].is_null(),
            "手机手里还有凭证：不补发 token（沿用原来那一把）"
        );
    }


    /// 电脑端设置页改开关：**只带一项时也要联动关掉另一项**——配置与运行标志都要关。
    ///
    /// 回归护栏（用户实测）：电脑端两个都打开后关掉「自动粘贴」，「自动发送」只是被置灰、
    /// 并没有关；再打开「自动粘贴」时，「自动发送」又是开着的。根因是重构时只关了配置、
    /// 漏了运行标志——而电脑端设置页读的正是运行标志。
    #[test]
    fn test_pc_toggle_dependency_applies_to_runtime_flag() {
        let (state, _code) = test_state();
        // 生效值 = 用户意愿 && 当前会话能不能注入。CI 是没有显示器的无头环境
        // （injection_backend() == Unsupported）⇒ 这里必须对着能力位断言，
        // 不能硬编码为 true——2026-10-18 CI 上就是这么挂的
        let supported = crate::paste::auto_paste_supported();
        state.apply_prefs(Some(true), Some(true));
        assert_eq!(
            state.auto_paste.load(Ordering::Relaxed),
            supported,
            "自动粘贴的生效值 = 意愿 && 能力"
        );
        assert_eq!(
            state.auto_send.load(Ordering::Relaxed),
            supported,
            "两个都打开时，自动发送同样受能力约束"
        );

        // 设置页只改「自动粘贴」（apply_prefs(Some(false), None) 正是那条路径）
        state.apply_prefs(Some(false), None);
        {
            let cfg = state.config.lock().unwrap();
            assert!(!cfg.auto_paste && !cfg.auto_send, "配置里要联动关");
        }
        assert!(
            !state.auto_send.load(Ordering::Relaxed),
            "运行标志也必须联动关——设置页读的是它（这条就是用户实测的 bug）"
        );

        // 再把粘贴打开：发送应保持关，不会自己变回开（与手机端一致）
        state.apply_prefs(Some(true), None);
        assert_eq!(state.auto_paste.load(Ordering::Relaxed), supported);
        assert!(
            !state.auto_send.load(Ordering::Relaxed),
            "粘贴重新打开后，发送仍是关"
        );
    }

    /// 手机端改「自动粘贴 / 自动发送」：常用设备能改且当场落盘并回推；
    /// 临时访客改不动电脑设置，但照样会收到当前值（界面不会停在假状态）。
    #[test]
    fn test_set_prefs_requires_primary_and_syncs_back() {
        // 收齐 hello_ack 与 prefs（两者紧挨着发出，顺序不敏感）
        fn read_ack_and_prefs(
            reader: &mut BufReader<TcpStream>,
        ) -> (serde_json::Value, serde_json::Value) {
            let mut ack = None;
            let mut prefs = None;
            for _ in 0..8 {
                let v = read_json(reader);
                match v["type"].as_str() {
                    Some("hello_ack") => ack = Some(v),
                    Some("prefs") => prefs = Some(v),
                    _ => {}
                }
                if ack.is_some() && prefs.is_some() {
                    break;
                }
            }
            (
                ack.expect("应收到 hello_ack"),
                prefs.expect("连上后应收到 prefs"),
            )
        }

        let (state, code) = test_state();
        state.set_remember_next(true);
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-p","device_name":"小米14"}),
        );
        let (ack, p0) = read_ack_and_prefs(&mut reader);
        assert_eq!(ack["session"], "primary");
        assert_eq!(p0["auto_paste"], true, "默认开启自动粘贴");
        assert_eq!(p0["auto_send"], false, "默认关闭自动发送");

        // 成对同步：粘贴开着时打开发送（手机端会算出 {true,true} 整对发）
        sock.write_all(b"{\"type\":\"set_prefs\",\"auto_paste\":true,\"auto_send\":true}\n")
            .expect("write");
        let p1 = read_until(&mut reader, "prefs");
        assert_eq!(p1["auto_paste"], true);
        assert_eq!(p1["auto_send"], true);

        // 关掉自动粘贴（手机端发的是整对 {false,false}）
        sock.write_all(b"{\"type\":\"set_prefs\",\"auto_paste\":false,\"auto_send\":false}\n")
            .expect("write");
        let p2 = read_until(&mut reader, "prefs");
        assert_eq!(p2["auto_paste"], false);
        assert_eq!(p2["auto_send"], false, "粘贴关 → 发送同时关");
        {
            let cfg = state.config.lock().unwrap();
            assert!(!cfg.auto_paste && !cfg.auto_send, "要落盘");
        }
        // 运行标志也必须一起关：电脑端设置页读的是它（只关配置就会出现"勾着但置灰"、
        // 再把粘贴打开时又变回开着——用户实测的正是这条）
        assert!(
            !state.auto_paste.load(Ordering::Relaxed),
            "运行态：自动粘贴要关"
        );
        assert!(
            !state.auto_send.load(Ordering::Relaxed),
            "运行态：自动发送必须联动关掉"
        );

        // 防御：万一有客户端发来非法组合（粘贴关却要打开发送），电脑端也必须存成关
        sock.write_all(b"{\"type\":\"set_prefs\",\"auto_paste\":false,\"auto_send\":true}\n")
            .expect("write");
        let p3 = read_until(&mut reader, "prefs");
        assert_eq!(p3["auto_paste"], false);
        assert_eq!(p3["auto_send"], false, "非法组合要被防御性联动纠正");
        {
            let cfg = state.config.lock().unwrap();
            assert!(!cfg.auto_paste && !cfg.auto_send, "非法组合不能落盘");
        }
        assert!(
            !state.auto_send.load(Ordering::Relaxed),
            "运行态也不能被非法组合打开"
        );

        // 临时访客：改不动，但会收到当前值（不静默失败）
        drop(sock);
        drop(reader);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let code2 = state.config.lock().unwrap().regenerate_code();
        state.set_code(code2.clone());
        state.set_remember_next(false);
        let (mut sock2, mut reader2) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock2,
            serde_json::json!({"type":"hello","code":code2,"client_id":"cid-t","device_name":"客人手机"}),
        );
        let ack2 = read_until(&mut reader2, "hello_ack");
        assert_eq!(ack2["session"], "transient", "没勾记住 → 临时身份");
        // 临时访客改不动：请求被忽略、配置不变，也不会收到回推
        // （手机端在临时身份下根本不显示这两个开关，所以无需回推纠正）
        sock2
            .write_all(b"{\"type\":\"set_prefs\",\"auto_paste\":true,\"auto_send\":true}\n")
            .expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !state.config.lock().unwrap().auto_paste,
            "配置不能被临时访客改动"
        );
    }

    /// 反复"解绑 → 设为常用设备 → 再解绑"：每一次都必须通知到手机。
    ///
    /// 回归护栏：历史上同一条连接里只有**第一次**解绑发得出去（一个只置位、
    /// 不复位的闩），于是重复解绑时手机端标签永远停在"常用设备"。
    #[test]
    fn test_repeated_unbind_notifies_every_time() {
        let (state, code) = test_state();
        state.set_remember_next(true);
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-r","device_name":"测试手机"}),
        );
        assert_eq!(read_until(&mut reader, "hello_ack")["session"], "primary");

        for round in 1..=3 {
            state.unbind_primary();
            let ev = read_until(&mut reader, "session_downgraded");
            assert!(
                ev["resume_token"].is_string(),
                "第 {round} 次解绑也必须通知，且带续连凭证"
            );

            // 手机被重新设为常用设备（等价于设置页点「设为常用设备」）
            state.promote_current_session().expect("设为常用设备应当成功");
            let promoted = read_until(&mut reader, "promoted");
            assert_eq!(promoted["type"], "promoted");
            sock.write_all(b"{\"type\":\"promoted_ack\"}\n").expect("ack");
        }
    }

    /// 拒绝类应答必须带上 device_id：手机据此核对"是不是我那台电脑在拒我"，
    /// 否则一台凑巧占了旧地址的陌生设备就能让手机把绑定删掉。
    #[test]
    fn test_rejection_ack_carries_device_id() {
        let (state, _code) = test_state();
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));

        // 拿一个编造出来的 token 去连：必然被拒
        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","token":"00".repeat(32),
                               "client_id":"cid-x","device_name":"陌生手机"}),
        );
        let ack = read_until(&mut reader, "hello_ack");
        assert_eq!(ack["ok"], false);
        assert_eq!(ack["status"], "unauthorized");
        let reported = ack["device_id"].as_str().unwrap_or_default();
        assert!(
            !reported.is_empty() && reported == state.device_identity().0,
            "拒绝应答必须带上本机 device_id（手机要核对拒绝方是谁），实际 ack = {ack}"
        );
    }

    /// 临时连接"换网后又恢复"的续连：旧连接**不给电脑任何告别信号**（没 FIN、
    /// 电脑还把它算在席位上），此时立刻续连也必须成功——电脑要把同一台手机的
    /// 旧连接当僵尸踢掉，而不是回「占用中」（那会让手机把窗口作废、逼用户重扫）。
    #[test]
    fn test_transient_resume_replaces_own_stale_connection() {
        let (state, code) = test_state();
        let (mut first, mut first_reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut first,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-t","device_name":"临时手机"}),
        );
        let ack = read_until(&mut first_reader, "hello_ack");
        assert_eq!(ack["session"], "transient");
        let resume_token = ack["resume_token"]
            .as_str()
            .expect("临时连接应下发续连凭证")
            .to_string();

        // 关键：**不 drop 旧连接**（不给 FIN），模拟"手机换网、旧连接还挂在电脑上"
        assert!(state.session.lock().unwrap().is_some(), "席位仍被旧连接占着");

        let (mut second, mut second_reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut second,
            serde_json::json!({"type":"hello","resume":resume_token,
                               "client_id":"cid-t","device_name":"临时手机"}),
        );
        let ack2 = read_until(&mut second_reader, "hello_ack");
        assert_eq!(ack2["ok"], true, "同一台手机的僵尸连接应被踢旧接新");
        assert_eq!(ack2["session"], "transient");

        let ended = read_until(&mut first_reader, "session_ended");
        assert_eq!(ended["reason"], "replaced", "旧连接应收到被替换的通知");
    }

    /// 心跳回报权威身份：常用设备 primary、解绑降级后 transient。
    /// 手机端据此自愈——就算那条身份推送丢了，一次心跳内也能纠正标签。
    #[test]
    fn test_pong_reports_authoritative_identity() {
        let (state, code) = test_state();
        state.set_remember_next(true);
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-id","device_name":"测试手机"}),
        );
        assert_eq!(read_until(&mut reader, "hello_ack")["session"], "primary");

        sock.write_all(b"{\"type\":\"ping\"}\n").expect("ping");
        assert_eq!(
            read_until(&mut reader, "pong")["session"],
            "primary",
            "常用设备的心跳应回报 primary"
        );

        state.unbind_primary();
        read_until(&mut reader, "session_downgraded");
        sock.write_all(b"{\"type\":\"ping\"}\n").expect("ping");
        assert_eq!(
            read_until(&mut reader, "pong")["session"],
            "transient",
            "降级后心跳必须回报临时设备身份"
        );
    }

    /// 发现门控：**窗口期内有效的临时续连凭证**也应得到应答。
    /// 临时设备只记得 IP+端口，电脑换 IP 时这是它唯一的找回路径；
    /// 无效/无关/已作废的凭证仍然一律隐身。
    #[test]
    fn test_discovery_accepts_valid_transient_resume() {
        use crate::config::sha256_hex;
        use crate::discovery::build_response;

        let (state, _code) = test_state();
        let req = |client: &str, proof: &str| {
            serde_json::json!({
                "type": "vl_disc_req", "protocol": 1,
                "client_id": client, "proof": proof
            })
            .to_string()
        };

        assert!(
            build_response(&state, req("cid-t", &"00".repeat(32)).as_bytes()).is_none(),
            "没有任何凭证时必须隐身"
        );

        let (token, ttl) = state
            .issue_transient_resume("cid-t", "临时手机")
            .expect("应签发续连凭证");
        assert!(ttl > 0);
        let proof = sha256_hex(&token);

        assert!(
            build_response(&state, req("cid-t", &proof).as_bytes()).is_some(),
            "有效临时凭证应得到应答（临时设备换 IP 才找得回来）"
        );
        assert!(
            build_response(&state, req("cid-x", &proof).as_bytes()).is_none(),
            "client_id 不符仍隐身"
        );
        assert!(
            build_response(&state, req("cid-t", &sha256_hex("nope")).as_bytes()).is_none(),
            "proof 错误仍隐身"
        );

        // 主人断开 / 手机 bye → 记录作废，立刻回到隐身
        state.clear_transient_resume("cid-t");
        assert!(
            build_response(&state, req("cid-t", &proof).as_bytes()).is_none(),
            "作废后的临时凭证必须隐身"
        );
    }

    /// 接管的完整协议链路：临时手机占着席位时，已绑定手机凭长期凭证握手会进入
    /// "等待接管"（回 occupied 但连接保持），发 `takeover` 后拿到席位，临时方
    /// 收到 `session_ended{owner_takeover}`。
    ///
    /// 对应手机界面上的"点两下连接"：第一下发现被占用（自动重试随即停止），
    /// 第二下才是接管——协议层只认这一条路径，临时方无法篡位。
    #[test]
    fn test_takeover_lets_bound_phone_kick_transient_occupant() {
        let (state, code) = test_state();

        // 1) 先绑一部手机（拿长期凭证），然后让它断开，把席位空出来
        state.set_remember_next(true);
        let (mut bound, mut bound_reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut bound,
            serde_json::json!({"type":"hello","code":code,"client_id":"cid-bound","device_name":"绑定手机"}),
        );
        let ack = read_until(&mut bound_reader, "hello_ack");
        assert_eq!(ack["session"], "primary");
        let token = ack["token"].as_str().expect("绑定应下发 token").to_string();
        drop(bound);
        drop(bound_reader);
        std::thread::sleep(std::time::Duration::from_millis(300));

        // 2) 临时手机扫码占住席位
        let (mut transient, mut transient_reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut transient,
            serde_json::json!({"type":"hello","code":state.get_code(),"client_id":"cid-guest","device_name":"临时手机"}),
        );
        assert_eq!(
            read_until(&mut transient_reader, "hello_ack")["session"],
            "transient"
        );

        // 3) 绑定手机凭凭证回来：席位被占 → 回 occupied 且连接保持（等待接管）
        let (mut back, mut back_reader) = connect_as_phone(spawn_server(state.clone()));
        send_hello(
            &mut back,
            serde_json::json!({"type":"hello","token":token,"client_id":"cid-bound","device_name":"绑定手机"}),
        );
        let occupied = read_until(&mut back_reader, "hello_ack");
        assert_eq!(occupied["ok"], false);
        assert_eq!(occupied["status"], "occupied", "席位被占应回 occupied");
        assert!(state.session.lock().unwrap().is_some(), "被占的连接应保持");

        // 4) 发 takeover → 拿到席位；临时方收到 owner_takeover 后连接结束
        back.write_all(b"{\"type\":\"takeover\"}\n").expect("takeover");
        let ack_takeover = read_until(&mut back_reader, "takeover_ack");
        assert_eq!(ack_takeover["ok"], true, "绑定手机应接管成功");

        let ended = read_until(&mut transient_reader, "session_ended");
        assert_eq!(
            ended["reason"], "owner_takeover",
            "临时方应收到主人接管的通知"
        );

        // 5) 接管后席位归绑定手机，托盘/状态同步
        let (_, name, is_primary) = state.session_summary();
        assert_eq!(name.as_deref(), Some("绑定手机"));
        assert!(is_primary, "接管后应是常用设备席位");
    }

    /// 没有连接时转正应报错（界面据此提示）
    #[test]
    fn test_promote_requires_live_session() {
        let (state, _code) = test_state();
        assert!(state.promote_current_session().is_err());
    }

    /// 陌生/伪造的续连凭证必须被拒，且不会让连接进入席位
    #[test]
    fn test_bogus_resume_rejected() {
        let (state, _code) = test_state();
        let (mut sock, mut reader) = connect_as_phone(spawn_server(state.clone()));

        send_hello(
            &mut sock,
            serde_json::json!({"type":"hello","resume":"deadbeef","client_id":"cid-x","device_name":"陌生手机"}),
        );
        let ack = read_until(&mut reader, "hello_ack");
        assert_eq!(ack["ok"], false);
        assert_eq!(ack["status"], "unauthorized");
        assert!(state.session.lock().unwrap().is_none(), "不应占用席位");
    }
}
