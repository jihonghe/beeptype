use crate::config::Config;
use crate::protocol::{Capabilities, now_ms, Message, MAX_LINE_BYTES};
use log::{info, warn};
use std::collections::HashMap;
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
use tauri_plugin_notification::NotificationExt;

/// 鉴权失败冷却配置
const MAX_AUTH_FAILS: u32 = 5;
const COOLDOWN_SECS: u64 = 60;
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
    pub auth_fails: Arc<Mutex<HashMap<IpAddr, u32>>>,
    pub auth_cooldowns: Arc<Mutex<HashMap<IpAddr, Instant>>>,
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
            auth_fails: self.auth_fails.clone(),
            auth_cooldowns: self.auth_cooldowns.clone(),
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
            auth_fails: Arc::new(Mutex::new(HashMap::new())),
            auth_cooldowns: Arc::new(Mutex::new(HashMap::new())),
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

    /// 检查 IP 是否在冷却期
    pub fn is_in_cooldown(&self, ip: IpAddr) -> bool {
        if let Ok(cooldowns) = self.auth_cooldowns.lock() {
            if let Some(start) = cooldowns.get(&ip) {
                return start.elapsed().as_secs() < COOLDOWN_SECS;
            }
        }
        false
    }

    /// 记录鉴权失败
    pub fn record_auth_fail(&self, ip: IpAddr) {
        if let Ok(mut fails) = self.auth_fails.lock() {
            let count = fails.entry(ip).or_insert(0);
            *count += 1;
            if *count >= MAX_AUTH_FAILS {
                if let Ok(mut cooldowns) = self.auth_cooldowns.lock() {
                    cooldowns.insert(ip, Instant::now());
                }
                *count = 0;
            }
        }
    }

    /// 清除鉴权失败计数
    pub fn clear_auth_fails(&self, ip: IpAddr) {
        if let Ok(mut fails) = self.auth_fails.lock() {
            fails.remove(&ip);
        }
        if let Ok(mut cooldowns) = self.auth_cooldowns.lock() {
            cooldowns.remove(&ip);
        }
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
    /// 扫码只会收到 bad_code，连续失败还会触发冷却。已连接时绝不换码。
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
            } => self.handle_hello(
                code,
                token,
                resume,
                client_id,
                device_name,
                hardware_id,
                peer_ip,
                ctx,
            ),

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
                let session = if self.is_session_holder(ctx) {
                    if ctx.waiting_takeover {
                        None
                    } else if ctx.identity.load(Ordering::SeqCst) {
                        Some("primary".to_string())
                    } else {
                        Some("transient".to_string())
                    }
                } else {
                    None
                };
                Some(Message::pong(session.as_deref()))
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
        peer_ip: IpAddr,
        ctx: &mut ConnCtx,
    ) -> Option<Message> {
        // 冷却优先
        if self.is_in_cooldown(peer_ip) {
            warn!("IP in cooldown: {}", peer_ip);
            return Some(Message::hello_ack_err(
                "cooldown",
                "尝试过于频繁，请 60 秒后再试",
            ));
        }

        // 记下硬件指纹：设为常用设备时写进记录（"重装后仍认得出"靠它）
        ctx.hardware_id = hardware_id.clone();

        let caps = Capabilities {
            auto_paste: self.auto_paste.load(Ordering::Relaxed),
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
            if session.is_some() {
                // 席位被占（例如常用设备正在用）：按既有规则拒绝，连接不保持
                return Some(Message::hello_ack_err("occupied", "电脑使用中"));
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
                self.record_auth_fail(peer_ip);
                warn!("Token rejected from {}", peer_ip);
                return Some(Message::hello_ack_err(
                    "unauthorized",
                    "设备未授权，请在电脑上重新配对",
                ));
            }
            self.clear_auth_fails(peer_ip);

            let (device_id, pc_name) = self.device_identity();
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
                    Some(Message::hello_ack_err("occupied", "电脑使用中"))
                }
            }
        } else if let Some(code) = code {
            // ---- 码分支：扫码 / 手动输码 ----
            let code_ok = {
                let config = self.config.lock().unwrap();
                config.verify_code(&code)
            };
            if !code_ok {
                self.record_auth_fail(peer_ip);
                warn!("Bad code from {}", peer_ip);
                return Some(Message::hello_ack_err("bad_code", "配对码错误或已过期"));
            }
            self.clear_auth_fails(peer_ip);

            let mut session = self.session.lock().unwrap();
            if session.is_some() {
                // 码对但席位占用：不认证、不绑定，直接拒绝
                return Some(Message::hello_ack_err("occupied", "电脑使用中"));
            }

            // 席位空闲：判定身份。
            //  - 勾选"记住"且尚无常用设备：绑定为新常用设备，下发新 token
            //  - 来者 client_id 就是已绑定常用设备（常用设备手动断开后扫码回来）：
            //    直接恢复常用设备席位，token 不下发（手机端本就保存着）
            //  - 其余：临时设备
            // 无 client_id 的旧客户端不允许占常用设备席位
            let remember = self.remember_next.load(Ordering::Relaxed);
            let (is_primary, token_out, new_code, returning_primary) = {
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
                let bind = remember && !config.has_primary() && !client_id.is_empty();
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
                (bind || returning, token_out, new_code, returning)
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
            auto_hide_after_pairing(&self.app_handle);
            info!(
                "Code pairing OK from {} ({})",
                peer_ip,
                if is_primary {
                    if returning_primary { "returning primary" } else { "primary" }
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
            self.record_auth_fail(peer_ip);
            warn!("Empty credentials from {}", peer_ip);
            Some(Message::hello_ack_err("unauthorized", "握手缺少凭证"))
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
                .title("BeeType")
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
        // 2. 当前平台支持全局按键模拟（X11/Windows/macOS；Wayland 回退纯剪贴板）；
        // 3. BeeType 自己的窗口没有焦点，避免文字糊进配对/状态窗口。
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
            let focus_before = crate::paste::focused_window_id();
            match crate::paste::simulate_paste() {
                Ok(mut enigo) => {
                    pasted = true;
                    // 自动发送：粘贴成功后模拟 Enter，复用同一条 enigo 连接
                    let auto_send = self.auto_send.load(Ordering::Relaxed);
                    if auto_send && crate::paste::auto_send_supported() {
                        let focus_after = crate::paste::focused_window_id();
                        if matches!((focus_before, focus_after), (Some(a), Some(b)) if a != b) {
                            // 用户在这几十/几百毫秒里切了窗口：不发回车（文字已粘贴，
                            // 需要的话由用户自己按回车），只记日志不打扰
                            info!("Auto-send skipped: focus window changed during paste");
                        } else {
                            match crate::paste::simulate_enter(&mut enigo, text.chars().count()) {
                                Ok(()) => info!("Auto-send: Enter sent"),
                                Err(e) => warn!("Auto-send failed: {}", e),
                            }
                        }
                    }
                }
                Err(e) => warn!("Auto-paste failed (text stays in clipboard): {}", e),
            }
        } else if enabled && own_window_focused {
            info!("Skip auto-paste: BeeType window is focused");
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
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
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
                    "timeout /t {RESTART_DELAY_SECS} /nobreak >nul & start \"\" \"%BEETYPE_RESTART_EXE%\""
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
    cmd.env("BEETYPE_RESTART_EXE", exe);
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

/// 获取局域网 IP（优先 IPv4）
pub fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// 生成 QR 码内容字符串
/// 格式：beetype://<ip>:<port>?code=<code>
pub fn qr_content(ip: &str, port: u16, code: &str) -> String {
    format!("beetype://{}:{}?code={}", ip, port, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_content() {
        let qr = qr_content("192.168.1.100", 8765, "123456");
        assert_eq!(qr, "beetype://192.168.1.100:8765?code=123456");
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
        let exe = Path::new("/usr/bin/beetype");
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
                !script.contains("/usr/bin/beetype"),
                "路径不能拼进脚本文本"
            );
            assert_eq!(args[2].to_string_lossy(), "/usr/bin/beetype");
        }
        #[cfg(windows)]
        {
            assert_eq!(program, "cmd");
            let script = args.last().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
            assert!(script.contains("timeout /t"), "应先等旧进程退出再启动");
            assert!(script.contains("%BEETYPE_RESTART_EXE%"), "路径应经环境变量传入");
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

    /// 反复"解绑 → 设为常用设备 → 再解绑"：每一次都必须通知到手机。
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
