use crate::config::Config;
use crate::protocol::{Capabilities, now_ms, Message, MAX_LINE_BYTES};
use log::{info, warn};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tauri::menu::MenuItem;
use tauri::{AppHandle, Emitter, Manager};
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
/// 同一主力的旧僵尸连接被新连接替换
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
    pub is_primary: bool,
    /// 非 0 表示持有者被要求结束，连接线程读到后发 session_ended 并退出
    pub kick: Arc<AtomicU8>,
    /// 解绑降级标志：置位后连接线程发一次 session_downgraded，会话以临时身份继续
    pub downgrade: Arc<AtomicBool>,
    /// 连接 socket 的克隆：踢人时 shutdown(Read) 唤醒阻塞中的 read_line，
    /// 让 session_ended 与席位释放立刻完成，而不用等对端下一条心跳（最长 5s）
    pub wake: Mutex<Option<TcpStream>>,
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
fn send_session_ended(writer: &mut TcpStream, name: &str, reason: u8) {
    let ended = Message::SessionEnded {
        reason: kick_reason(reason).to_string(),
    };
    if let Ok(encoded) = ended.encode() {
        let _ = writer.write_all(encoded.as_bytes());
        let _ = writer.flush();
    }
    warn!("Session ended for {} ({})", name, kick_reason(reason));
}

/// 通知席位持有者：主力已解绑，连接降级为临时（会话保持）
fn send_session_downgraded(writer: &mut TcpStream, name: &str) {
    if let Ok(encoded) = Message::SessionDowngraded.encode() {
        let _ = writer.write_all(encoded.as_bytes());
        let _ = writer.flush();
    }
    info!("Session downgraded to transient for {}", name);
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
    is_primary: bool,
    /// 已认证主力但席位被占，连接保持、等待 takeover
    waiting_takeover: bool,
    /// 本连接持有席位时对应的 kick 标志（用于与全局席位比对）
    kick: Arc<AtomicU8>,
    /// 本连接持有席位时对应的解绑降级标志（与席位共享）
    downgrade: Arc<AtomicBool>,
    /// 本连接 socket 的克隆，admit 时存入席位供远端踢人唤醒
    wake: Option<TcpStream>,
    last_active: Instant,
    /// 收到 bye：回完 ack 即退出
    closing: bool,
}

impl Default for ConnCtx {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            name: String::new(),
            authed: false,
            is_primary: false,
            waiting_takeover: false,
            kick: Arc::new(AtomicU8::new(0)),
            downgrade: Arc::new(AtomicBool::new(false)),
            wake: None,
            last_active: Instant::now(),
            closing: false,
        }
    }
}

impl ConnCtx {
    fn admit(
        &mut self,
        client_id: String,
        name: String,
        is_primary: bool,
        kick: Arc<AtomicU8>,
        downgrade: Arc<AtomicBool>,
    ) {
        self.client_id = client_id;
        self.name = name;
        self.authed = true;
        self.is_primary = is_primary;
        self.waiting_takeover = false;
        self.kick = kick;
        self.downgrade = downgrade;
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

/// 主力 token 认证通过后的席位仲裁结果（纯逻辑，便于单测）
#[derive(Debug, PartialEq)]
enum PrimaryArbitration {
    /// 席位空闲，直接接入
    Admit,
    /// 被同一主力的旧连接占着（僵尸重连），踢旧接新
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

/// 服务端共享状态
#[derive(Clone)]
pub struct ServerState {
    pub state: Arc<Mutex<ConnState>>,
    pub auth_fails: Arc<Mutex<HashMap<IpAddr, u32>>>,
    pub auth_cooldowns: Arc<Mutex<HashMap<IpAddr, Instant>>>,
    pub app_handle: AppHandle,
    pub port: u16,
    pub pairing_code: Arc<Mutex<String>>,
    /// 持久配置（device_id/device_name/primary）
    pub config: Arc<Mutex<Config>>,
    /// 唯一席位
    pub session: Arc<Mutex<Option<ActiveSession>>>,
    /// 配对窗口"记住此手机"勾选：对下一次码配对生效；已绑主力时忽略
    pub remember_next: Arc<AtomicBool>,
    /// 托盘菜单第一行状态项（setup 建完托盘后注入）
    pub status_item: Arc<Mutex<Option<MenuItem<tauri::Wry>>>>,
    /// 托盘"断开当前手机"项（有会话时可点）
    pub disconnect_item: Arc<Mutex<Option<MenuItem<tauri::Wry>>>>,
    /// 自动粘贴开关（实际生效值：用户开关 && 平台支持）
    pub auto_paste: Arc<AtomicBool>,
    /// 自动发送开关（粘贴后模拟 Enter）。实际生效值：auto_paste 生效 && 平台支持
    pub auto_send: Arc<AtomicBool>,
    /// 手机连上后是否自动隐藏主窗口到托盘（用户设置项）
    pub auto_hide: Arc<AtomicBool>,
    /// 临时会话的续连记录（仅内存，只留最近一条；窗口期 0 = 关闭续连）
    pub transient_resume: Arc<Mutex<Option<TransientResume>>>,
}

impl ServerState {
    pub fn new(
        app_handle: AppHandle,
        port: u16,
        code: String,
        auto_paste: bool,
        auto_send: bool,
        auto_hide: bool,
        config: Config,
    ) -> Self {
        // 扫码默认就是临时连接：勾选"绑为主力"必须是用户的主动行为
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
        }
    }

    /// setup 建完托盘后注入菜单项句柄
    pub fn set_menu_items(&self, disconnect: Option<MenuItem<tauri::Wry>>) {
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
                Some(a) if a.is_primary => {
                    (ConnState::Connected, format!("🟢 {}", a.name), true)
                }
                Some(a) => (
                    ConnState::Connected,
                    format!("🟢 {} · 临时", a.name),
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

    /// 席位摘要：(是否连接中, 手机名, 是否主力)
    pub fn session_summary(&self) -> (bool, Option<String>, bool) {
        let s = self.session.lock().unwrap();
        match s.as_ref() {
            Some(a) => (true, Some(a.name.clone()), a.is_primary),
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

    /// 设置页/托盘"解除绑定"：凭证立即作废；若主力正在线，连接降级为
    /// 临时连接继续服务（不断开）。手机端凭 session_downgraded 同步状态。
    pub fn unbind_primary(&self) {
        {
            let mut config = self.config.lock().unwrap();
            config.unbind_primary();
        }
        let mut session = self.session.lock().unwrap();
        if let Some(a) = session.as_mut() {
            if a.is_primary {
                a.is_primary = false;
                a.downgrade.store(true, Ordering::SeqCst);
            }
        }
        drop(session);
        self.publish_state();
        info!("Primary unbound; live connection downgraded to transient");
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
                // 临时会话仍在活跃：续连窗口往后推
                if !ctx.is_primary {
                    self.touch_transient_resume(&ctx.client_id);
                }
                self.handle_text(id, text)
            }

            Message::Ping => {
                ctx.last_active = Instant::now();
                if !ctx.is_primary {
                    self.touch_transient_resume(&ctx.client_id);
                }
                Some(Message::Pong)
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
                // 席位被占（例如主力正在用）：按既有规则拒绝，连接不保持
                return Some(Message::hello_ack_err("occupied", "电脑使用中"));
            }
            let kick = Arc::new(AtomicU8::new(0));
            let downgrade = Arc::new(AtomicBool::new(false));
            *session = Some(ActiveSession {
                client_id: client_id.clone(),
                name: name.clone(),
                is_primary: false,
                kick: kick.clone(),
                downgrade: downgrade.clone(),
                wake: Mutex::new(clone_wake(&ctx.wake)),
            });
            drop(session);

            ctx.admit(client_id, name.clone(), false, kick, downgrade);
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

        // ---- token 分支：持久主力免配重连 ----
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
                    let kick = Arc::new(AtomicU8::new(0));
                    let downgrade = Arc::new(AtomicBool::new(false));
                    *session = Some(ActiveSession {
                        client_id: client_id.clone(),
                        name: device_name.clone(),
                        is_primary: true,
                        kick: kick.clone(),
                        downgrade: downgrade.clone(),
                        wake: Mutex::new(clone_wake(&ctx.wake)),
                    });
                    drop(session);
                    ctx.admit(client_id, device_name, true, kick, downgrade);
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
                    // 同一主力的旧连接还挂着（僵尸重连）：要求旧连接退出，新连接接席
                    if let Some(old) = session.as_ref() {
                        kick_session(old, KICK_REPLACED);
                    }
                    let kick = Arc::new(AtomicU8::new(0));
                    let downgrade = Arc::new(AtomicBool::new(false));
                    *session = Some(ActiveSession {
                        client_id: client_id.clone(),
                        name: device_name.clone(),
                        is_primary: true,
                        kick: kick.clone(),
                        downgrade: downgrade.clone(),
                        wake: Mutex::new(clone_wake(&ctx.wake)),
                    });
                    drop(session);
                    ctx.admit(client_id, device_name, true, kick, downgrade);
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
                    // 席位被临时方占用：连接保持，等待手动接管
                    drop(session);
                    ctx.client_id = client_id;
                    ctx.name = device_name;
                    ctx.authed = true;
                    ctx.is_primary = true;
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
            //  - 勾选"记住"且尚无主力：绑定为新主力，下发新 token
            //  - 来者 client_id 就是已绑定主力（主力机手动断开后扫码回来）：
            //    直接恢复主力席位，token 不下发（手机端本就保存着）
            //  - 其余：临时连接
            // 无 client_id 的旧客户端不允许占主力位
            let remember = self.remember_next.load(Ordering::Relaxed);
            let (is_primary, token_out, new_code, returning_primary) = {
                let mut config = self.config.lock().unwrap();
                // returning: client_id 匹配 OR hardware_id 匹配
                // 硬件指纹是关键——手机重装后 client_id 变了，但 hardware_id 不变，
                // 仍能被识别为"我认识这台手机"并恢复主力身份
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
            // 主力不需要——它本来就有长期 token。
            let resume_issue = if is_primary {
                None
            } else {
                self.issue_transient_resume(&client_id, &device_name)
            };

            let (device_id, pc_name) = self.device_identity();
            let kick = Arc::new(AtomicU8::new(0));
            let downgrade = Arc::new(AtomicBool::new(false));
            *session = Some(ActiveSession {
                client_id: client_id.clone(),
                name: device_name.clone(),
                is_primary,
                kick: kick.clone(),
                downgrade: downgrade.clone(),
                wake: Mutex::new(clone_wake(&ctx.wake)),
            });
            drop(session);

            ctx.admit(client_id, device_name, is_primary, kick, downgrade);
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

    fn device_identity(&self) -> (String, String) {
        let config = self.config.lock().unwrap();
        (config.device_id.clone(), config.device_name.clone())
    }

    /// 主力点击"接管电脑"
    fn handle_takeover(&self, ctx: &mut ConnCtx) -> Option<Message> {
        if !ctx.authed || !ctx.is_primary || !ctx.waiting_takeover {
            warn!("Takeover rejected: connection not eligible");
            return Some(Message::TakeoverAck { ok: false });
        }

        let mut session = self.session.lock().unwrap();
        // 通知当前持有者结束（若期间刚好已空出，则无人可踢）
        if let Some(cur) = session.as_ref() {
            kick_session(cur, KICK_TAKEOVER);
        }
        let kick = Arc::new(AtomicU8::new(0));
        let downgrade = Arc::new(AtomicBool::new(false));
        *session = Some(ActiveSession {
            client_id: ctx.client_id.clone(),
            name: ctx.name.clone(),
            is_primary: true,
            kick: kick.clone(),
            downgrade: downgrade.clone(),
            wake: Mutex::new(clone_wake(&ctx.wake)),
        });
        drop(session);

        ctx.waiting_takeover = false;
        ctx.kick = kick;
        ctx.downgrade = downgrade;
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
fn auto_hide_after_pairing(app: &AppHandle) {
    let state = app.state::<Arc<Mutex<ServerState>>>();
    let server = state.lock().unwrap();
    let _ = app.emit("pairing-show", ());
    if !server.auto_hide.load(Ordering::Relaxed) {
        return;
    }
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(900));
        // 隐藏前再查一次：用户可能在 0.9s 内把设置关掉了
        let state = app.state::<Arc<Mutex<ServerState>>>();
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
pub fn serve(state: ServerState) {
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
fn handle_connection(stream: TcpStream, state: ServerState) {
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
    let mut writer = stream;

    let mut line = String::new();
    let mut downgrade_sent = false;
    'conn: loop {
        // 被踢/被替换：发 session_ended 后退出（清理段靠 kick 指针比对不会误清新席位）
        let kicked = ctx.kick.load(Ordering::SeqCst);
        if kicked != 0 {
            send_session_ended(&mut writer, &ctx.name, kicked);
            break 'conn;
        }

        // 解绑降级：发一次 session_downgraded，会话以临时身份继续（连接不断）
        // 心跳最长 5s 一轮，本检查每轮循环都会执行，降级通知延迟因此有界
        if !downgrade_sent && ctx.downgrade.load(Ordering::SeqCst) {
            downgrade_sent = true;
            ctx.is_primary = false;
            send_session_downgraded(&mut writer, &ctx.name);
        }

        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // shutdown(Read) 唤醒的踢人路径也表现为 EOF：先查 kick 再下结论
                let kicked = ctx.kick.load(Ordering::SeqCst);
                if kicked != 0 {
                    send_session_ended(&mut writer, &ctx.name, kicked);
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
                    match reply.encode() {
                        Ok(encoded) => {
                            if writer.write_all(encoded.as_bytes()).is_err() {
                                break 'conn;
                            }
                            let _ = writer.flush();
                        }
                        Err(e) => warn!("Encode error: {}", e),
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
            name: "主力".to_string(),
            is_primary: true,
            kick: kick.clone(),
            downgrade: Arc::new(AtomicBool::new(false)),
            wake: Mutex::new(None),
        };
        let transient_session = ActiveSession {
            client_id: "t1".to_string(),
            name: "临时".to_string(),
            is_primary: false,
            kick: Arc::new(AtomicU8::new(0)),
            downgrade: Arc::new(AtomicBool::new(false)),
            wake: Mutex::new(None),
        };

        // 空席：直接接入
        assert_eq!(arbitrate_primary(None, "p1"), PrimaryArbitration::Admit);
        // 同一主力旧连接：踢旧接新
        assert_eq!(
            arbitrate_primary(Some(&primary_session), "p1"),
            PrimaryArbitration::ReplaceSelf
        );
        // 临时方占用：等待接管
        assert_eq!(
            arbitrate_primary(Some(&transient_session), "p1"),
            PrimaryArbitration::WaitTakeover
        );
        // 另一 client_id 面对主力占用（理论上一台 PC 只有一个主力，逻辑仍应等待）
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

    #[test]
    fn test_kick_reason_mapping() {
        assert_eq!(kick_reason(KICK_TAKEOVER), "owner_takeover");
        assert_eq!(kick_reason(KICK_DISCONNECT), "owner_disconnect");
        assert_eq!(kick_reason(KICK_REPLACED), "replaced");
    }
}
