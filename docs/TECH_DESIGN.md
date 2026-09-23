# BeepType 技术设计文档

> **版本**：v0.2（与当前实现对齐）
> **日期**：2026-09-20
> **关联文档**：[PRD.md](./PRD.md)

本文描述**代码里实际存在的东西**：模块划分、数据结构、协议字段、状态机与常量取值，全部可在源码中对照。
v0.1 设计稿与最终实现的差异汇总在[第 9 节](#9-与-v01-设计稿的差异对照)。

---

## 1. 总体架构

```
┌──────────────────────────────┐                    ┌──────────────────────────────┐
│      手机端 BeepType App      │                    │     PC 端 BeepType Desktop    │
│         (Flutter)            │                    │        (Tauri + Rust)        │
│                              │                    │                              │
│  pages/  home_page           │   TCP  JSON 行协议  │  server.rs  TCP 服务 + 席位   │
│          scan_page           │ ←────────────────→ │  protocol.rs 消息编解码       │
│  services/ tcp_service       │   hello / text      │  paste.rs   按键模拟(enigo)   │
│            text_sender       │   ack / ping/pong   │  discovery.rs UDP 发现        │
│            storage_service   │   takeover / bye    │  config.rs  身份与配置        │
│            discovery_service │   session_ended     │  lib.rs     托盘/窗口/commands│
│                              │                    │  src/index.html 主窗口前端     │
│  平台通道 beeptype/device     │   UDP 发现请求/应答  │                              │
│  (ANDROID_ID / Keychain)     │ ←────────────────→ │  同上端口（默认 8765）         │
└──────────────────────────────┘                    └──────────────────────────────┘
```

两端各自持有一份长期身份与凭证：

| | 手机端 | PC 端 |
|---|---|---|
| 本机身份 | `client_id`（首启生成的 UUIDv4）+ `hardware_id`（Android ANDROID_ID / iOS Keychain UUID） | `device_id`（首启生成的 UUIDv4）+ `device_name`（hostname） |
| 对端凭证 | 已绑定电脑列表（含明文 token） | 唯一常用设备记录（含 `token_hash`） |
| 临时关系 | 不落**长期**凭证；仅落一条短期续连记录（窗口期内有效，过期即清） | 仅内存（续连记录的哈希）；进程退出即消失 |

**端口**：TCP 与 UDP 使用同一个端口号（`config.port`，默认 8765）。TCP 承载全部业务消息，UDP 只承载凭证门控的设备发现。

---

## 2. PC 端（Tauri 2 + Rust）

### 2.1 工程结构（实际文件）

```
desktop/
├── src/
│   └── index.html          # 单文件前端：主页（配对/状态）+ 设置页
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json     # 窗口 380x520、托盘图标、无 CSP
    ├── capabilities/default.json
    ├── icons/
    └── src/
        ├── main.rs         # 入口（Windows release 下隐藏控制台）
        ├── lib.rs          # Tauri 装配：托盘、commands、窗口联动
        ├── config.rs       # 身份、凭证、配置读写
        ├── protocol.rs     # 消息类型与编解码
        ├── server.rs       # TCP 服务、认证、席位状态机
        ├── clipboard.rs    # 剪贴板写入 + 读回校验
        ├── paste.rs        # 按键模拟（自动粘贴/自动发送）
        ├── winicon.rs      # 任务栏图标：自己写 _NET_WM_ICON（Linux/X11）
        └── discovery.rs    # UDP 凭证门控发现
```

> v0.1 设计中的 `tray.rs`、`qr.rs` 不存在：托盘与 QR 生成都在 `lib.rs`（QR 直接用 `qrcode` crate 渲染成 SVG 内联进 WebView）。`clipboard.rs` 现在是存在的，但它做的是"写入 + 读回校验"，**不是** v0.1 设想的"非文本保护"。

### 2.2 config.rs — 身份、凭证与配置

配置目录（`BEEPTYPE_HOME` 环境变量可覆盖，单测用）：

| 平台 | 路径 |
|---|---|
| Linux | `~/.config/beeptype/config.json` |
| macOS | `~/Library/Application Support/beeptype/config.json` |
| Windows | `%HOME%/AppData/Roaming/beeptype/config.json` |

```rust
pub struct Config {
    pub port: u16,                    // 默认 8765（TCP 与 UDP 同号）
    pub autostart: bool,              // 默认 false
    #[serde(default = "default_true")]
    pub auto_paste: bool,             // 默认 true（实际生效还需平台支持）
    #[serde(default)]
    pub auto_send: bool,              // 默认 false，依赖 auto_paste
    pub auto_hide: bool,              // 默认 true：手机连上后自动隐藏主窗口到托盘
    pub resume_ttl_min: u32,          // 默认 5：临时设备断线后免扫码续连的窗口（分钟），0=关闭
    pub code_hash: Option<String>,    // SHA-256(配对码)
    pub code_expires_at: Option<u64>, // 毫秒时间戳
    pub device_id: String,            // 本机 UUIDv4
    pub device_name: String,          // 默认 hostname
    pub primary: Option<PrimaryDevice>, // 唯一常用设备（None = 还没有常用设备）
}

pub struct PrimaryDevice {
    pub client_id: String,
    pub name: String,
    pub token_hash: String,           // SHA-256(token)，明文只在绑定时下发一次
    pub last_ip: String,
    pub paired_at: u64,
    pub last_seen: u64,
    pub hardware_id: Option<String>,  // 手机硬件指纹（可选，APP 重装后识别用）
}
```

关键方法：

| 方法 | 作用 |
|---|---|
| `load_or_create()` | 读配置；首次运行（或旧版升级）补建 `device_id`/`device_name` 并落盘 |
| `save()` | 先写 `config.json.tmp` 再 `rename`；文件 0600、目录 0700 |
| `regenerate_code()` | 生成新配对码（32 字节 `/dev/urandom` → 64 位 hex），写入 `code_hash` 与 5 分钟后的 `code_expires_at`，返回明文供二维码使用 |
| `verify_code(code)` | 先查过期时间，再比对 `SHA-256(code)` |
| `bind_primary(client_id, name, ip, hardware_id)` | 生成 32 字节 token，存 `token_hash`，返回明文 token（仅此一次下发） |
| `verify_token(client_id, token, name, ip)` | `client_id` 与 `token_hash` 双匹配；命中时刷新 `name`/`last_ip`/`last_seen` 并落盘 |
| `matches_discovery(client_id, proof)` | 纯比对不写盘（UDP 发现高频路径专用，避免反复落盘） |
| `unbind_primary()` | 清空常用设备记录，旧凭证立即失效 |

随机与哈希都是手写实现（`/dev/urandom` + `sha2`），没有引入 uuid/rand 依赖；旧版配置里的未知字段（如 `paired_device`）由 serde 默认忽略，向后兼容。

### 2.3 protocol.rs — 消息编解码

- 传输：每条消息一行 JSON + `\n`，UTF-8，单行上限 `MAX_LINE_BYTES = 256 * 1024`。
- 使用 serde 内部标记 `#[serde(tag = "type")]`；新字段全部 `#[serde(default)]`，旧客户端缺字段也能解析（有单测覆盖）。

消息类型（完整字段见[第 3 节](#3-通信协议)）：

| 变体 | 方向 |
|---|---|
| `hello` / `hello_ack` | 手机 → PC / PC → 手机 |
| `text` / `ack` | 手机 → PC / PC → 手机 |
| `ping` / `pong` | 手机 → PC / PC → 手机（`pong.session` 回报权威身份，见 §3.2） |
| `takeover` / `takeover_ack` | 手机 → PC / PC → 手机 |
| `bye` / `bye_ack` | 手机 → PC / PC → 手机 |
| `session_ended` | PC → 手机（被踢前通知） |
| `session_downgraded` | PC → 手机（解绑降级，会话保持；由设置命令线程当场发出） |

### 2.4 server.rs — TCP 服务、认证与席位

#### 常量

| 常量 | 值 | 含义 |
|---|---|---|
| `MAX_AUTH_FAILS` | 5 | 同 IP 累计鉴权失败次数 |
| `READ_TIMEOUT_SECS` | 5 | 所有连接的 socket 读超时（也是心跳的检测粒度） |
| `HEARTBEAT_TIMEOUT_SECS` | 15 | 已认证连接连续无消息多久后收尸（3 个心跳周期） |
| TCP keepalive | idle 10s / interval 5s / count 3 | 仅 Unix，兜底死连接（Windows 无此项，靠心跳） |

#### 席位（ActiveSession）

```rust
pub struct ActiveSession {
    pub client_id: String,
    pub name: String,
    pub is_primary: bool,
    pub kick: Arc<AtomicU8>,          // 0=正常，非 0=被要求结束（原因见 KICK_*）
    pub downgrade: Arc<AtomicBool>,   // 解绑降级标志
    pub wake: Mutex<Option<TcpStream>>, // socket 克隆，踢人时 shutdown(Read) 唤醒阻塞中的 read_line
}
```

一台 PC 同时只有一个席位（单光标，单会话）。持有者身份用 **`client_id` + `Arc::ptr_eq(kick)` 双比对**判定，因此新连接顶替席位后，旧连接的退出清理不会误清新席位。

`KICK_TAKEOVER = 1`（常用设备接管）、`KICK_DISCONNECT = 2`（托盘/窗口断开）、`KICK_REPLACED = 4`（同常用设备僵尸连接被替换），映射为 `session_ended.reason` 的 `owner_takeover` / `owner_disconnect` / `replaced`。

**写入统一走一把锁**（`SharedWriter = Arc<Mutex<TcpStream>>`）：连接线程的回复与**命令线程的推送**（例如"设为常用设备"要从设置页往活动连接里推凭证）共用同一个句柄，避免两条线程各持 socket 克隆并发写、把一条 JSON 行写交错。踢人用的 `wake` 仍是独立克隆（只做 `shutdown(Read)`）。

踢人不靠等对端心跳：`kick_session()` 置标志后立即 `shutdown(Shutdown::Read)`，让阻塞在 `read_line` 的连接线程马上醒来自行发 `session_ended` 并退出，延迟从最长 5s 降到毫秒级。

#### 连接生命周期

```
accept
  → 设置 TCP keepalive（Unix）
  → read_timeout = 5s
  → 循环：read_line
       ├─ 收到行 → handle_message → 需要回复则写回（写失败即退出）
       │            ├─ 首条必须是 hello（有效凭证），否则后续 text 一律拒绝
       │            └─ ctx.closing（收到 bye）→ 退出
       ├─ Ok(0) EOF → 若 kick 已置位则补发 session_ended；否则视为对端关闭
       ├─ 行 > 256KB → 直接断开（不做截断）
       ├─ 超时 → 已认证：15s 无消息则收尸；未认证：继续空转等待（由 keepalive/对端负责）
       └─ 其他错误 → 断开
  → 退出清理：仅当自己仍是席位持有者（client_id + kick 指针比对）才释放席位并刷新托盘状态
```

未认证连接不会因空闲被主动关闭（没有 hello 超时），实际约束来自 TCP keepalive 与对端行为。

#### 认证：三条分支

`handle_hello` 按凭证分流（**不限流**：配对码一次性 + 32 字节随机串，限流只会误伤反复扫到旧码的用户，v0.3.0 移除了原先的冷却机制）：

**resume 分支（临时会话断线续连）**

临时手机被 Android 杀进程后，凭配对时拿到的 `resume_token` 免扫码续接：

```
签发（码配对判定为临时时）：PC 生成 32 字节随机串 → 只在内存存 SHA-256 + client_id
                           + 到期时刻（只保留最近一条）→ 明文随 hello_ack 下发一次
续连（hello{resume, client_id}）：哈希与 client_id 都命中且未过期 → 接入临时席位，
                           并把窗口期往后推（滑动窗口）
                           **席位被同一 client_id 的旧连接占着 → 踢旧接新**（手机换网时
                           旧连接死在本地、发不出 FIN，电脑只能等 15s 心跳超时；
                           不这样做就会回「占用中」，手机据此作废窗口、逼用户重扫）
                           席位被别的 client_id 占着 → 回 occupied（不保持连接）
失效：TTL 到期 / PC 进程退出 / 主人点托盘"断开当前手机" / 手机端发 bye
     （席位被占时按既有规则拒绝：临时续连不享受常用设备待遇）

降级补发：主人解绑常用设备时，那条在线连接会降级为临时（`session_downgraded`）——
它同样是"临时会话"，因此**必须一并补发续连凭证**并放进该消息（字段 `resume_token` /
`resume_expires_in`）。这一步曾经被漏掉，导致"降级来的临时会话"被杀进程后回不来
（扫码来的临时设备却正常），用户实测发现。
```

**设置一改当场生效**：`set_resume_ttl` 改完配置后调用 `sync_transient_keepalive()`——若当前有在线的**临时**席位，开启/调长就当场签发并推送 `resume_grant`（带 token），关闭就作废 PC 侧记录并推送不带 token 的 `resume_grant`（手机据此清掉本地记录）。没有这一步的话，设置只在"签发凭证那一刻"被读取，改了要等下次配对或降级才生效（用户实测反馈）。

窗口期由设置项 `resume_ttl_min`（默认 5 分钟，可关闭）决定，每次心跳与发文字都会刷新——
"人还在用就一直有效，停手超过窗口才失效"。窗口时长通过 `hello_ack.resume_expires_in`
告知手机（PC 是权威方）。测试可用环境变量 `BEEPTYPE_RESUME_TTL_SECS` 覆盖（不进 UI）。

**身份标志与降级/升级通知（A+C 两处修复）**

席位的身份不是散落的多个 bool，而是**一个跨线程共享的原子量**：`ActiveSession.identity`
（true = 常用设备）。命令线程写它（解绑降级 / 设为常用设备），连接线程读它（决定心跳里
回报什么身份、临时设备要不要刷新续连窗口）。唯一真源，避免"两处各存一个、忘了同步"。

身份变化的消息**由设置命令线程当场发出**（`unbind_primary` / `promote_current_session`
都走席位里的 `SharedWriter`，与连接线程的回复共用同一把写锁，不会写交错）：

- 为什么不在连接线程里发：它平时阻塞在 socket 读上，靠轮询发现标志要等最长 5 秒
  （下一次心跳或 socket 读超时），用户看到的就是"手机上标签慢半拍"。
- 为什么不能只置一个"发过了"的布尔闩：那条闩只置位、不复位，于是同一条连接里
  **只有第一次解绑发得出去**——"解绑 → 设为常用设备 → 再解绑"时，后面几次电脑端一声不吭，
  手机端标签永远停在"常用设备"。旧实现正是这样，用户实测发现。

另有一层兜底：心跳（`pong`）带回权威身份（`session: "primary" | "transient"`），手机端
发现与自己认知不一致就当场纠正——即使某次推送因为任何原因丢了，一个心跳周期（约 5 秒）
内标签也会自愈。只有真正持席的连接才给身份声明：等待接管、未认证的连接一律省略
（手机收到空值就忽略，绝不瞎猜）。

**token 分支（常用设备免配重连）**

1. `verify_token(client_id, token, ...)` 失败 → 回 `unauthorized`（不记失败、不限流：配对码一次性 + 32 字节随机串，限流只会误伤反复扫到旧码的用户）。
2. 按下述仲裁结果处理：席位空闲 → 直接接入（`session = primary`）；席位被**同一 `client_id`** 占着（僵尸重连）→ 踢旧接新；席位被他人（临时方）占用 → 回 `status = "occupied"`，**连接保持**并标记 `waiting_takeover`，等手机发 `takeover`。

**code 分支（扫码接入）**

1. `verify_code(code)` 失败 → 回 `bad_code`（同上，不限流）。
2. 席位已被占用 → 回 `occupied`，不认证、不绑定、**不换码**。
3. 席位空闲时判定身份：
   - `returning`：来者的 `client_id` 命中常用设备记录，**或** `hardware_id` 命中常用设备记录（APP 重装后 `client_id` 会变，硬件指纹不会）→ 直接恢复常用设备席位，**不下发 token**（手机本来就有）。**例外**：手机在 `hello` 里声明 `has_local_credential: false`（重装 / 清过数据）时**必须补发一个新 token** 并刷新绑定记录——只恢复席位而不给凭证，它什么都存不下，断线闲置一会儿又被推回扫码页（用户实测）。
   - `bind`：勾选了「记住这台手机」且还没有常用设备且有 `client_id` → 绑定为新常用设备并一次性下发 token。
   - 其余 → 临时会话（`session = "transient"`）：PC 端不落盘任何东西，但会签发一个**续连凭证**（见下）。
4. 无论哪种结果，配对码都立即 `regenerate_code()`（**一次性**，杜绝重放），并写回内存中的二维码内容。

两分支成功后都会：写席位、刷新托盘、通知前端（`pairing-show`）、日志记录。

#### `takeover`（常用设备接管被占用的电脑）

仅当连接状态为「已认证 + 是常用设备 + 正在等待接管」时才受理：踢掉当前持有者（`owner_takeover`）、把席位交给发起方、回 `takeover_ack{ok:true}`；否则回 `ok:false`。**临时方发起的 takeover 必然被拒**——协议层不存在手机端篡位的路径。

手机界面上没有单独的「接管」按钮：被占用时打字区给一句提示，用户点胶囊上的「**连接**」即发 `takeover`（`AuthMode == token` 才走这条）。少一个按钮、少一个词，行为反而更明确——接管永远是用户的显式动作。

#### 收到 `text` 的处理链

1. 非席位持有者 / 未认证 / 等待接管中的连接 → 回 `ack{ok:false, code:"not_connected"}`。
2. 文本 `trim()` 为空 → 回 `ack{ok:false, code:"empty_text"}`，不做任何剪贴板操作。
3. 写入剪贴板并**读回校验**（`clipboard.rs`）：读到别的内容（被其他进程抢先持有）就重写一次；写入报错或重写后仍不一致 → 弹系统通知，回 `ack{ok:false, code:"clipboard_error"}`。读不回来则按原样继续（fail-open，不因"校验不了"把能用的粘贴挡掉）。
4. 自动粘贴条件全部满足时才模拟粘贴：设置页开关生效 + 当前会话支持按键注入 + **BeepType 自己的窗口没有焦点**（避免把文字糊进配对窗口）。注入前记下焦点窗口，成功则 `pasted = true`，并交回 enigo 实例。
5. 自动发送：`auto_send` 生效且**焦点窗口未变**时，复用同一个 enigo 实例、等 `enter_delay(字符数)` 后模拟 Enter；焦点变了就跳过（文字已粘贴，由用户自己按回车），只记日志、不在手机端打扰用户。
6. 回 `ack{ok:true, pasted, srv_recv_ms}`（`srv_recv_ms` 是服务端收到消息的时间戳，供手机端算延迟）。

### 2.5 clipboard.rs — 写入与读回校验

```rust
pub fn write_verified(app: &AppHandle, text: &str) -> bool
```

写剪贴板是整条链上唯一"可能判断错"的环节：有的机器装着剪贴板管理器（Klipper / GPaste / CopyQ），可能在写入后立刻接管所有权，也可能写入被静默丢弃。这里不假定成功、也不按机器特调，而是当场验证：

| 读回结果 | 处理 |
|---|---|
| 与我们写入的一致 | 通过 |
| 是别的内容（被别人持有） | 重写一次再验；两次都不一致 → 判定写入失败 |
| 读不回来（无法判定） | 只记日志，按原样继续（fail-open） |

在 X11 上，我们持有所有权时读回是**本地内存读**（arboard 直接返回自己的数据），不产生额外的 X 往返、不会阻塞；只有所有权被抢走时才会真去读一次（arboard 内部上限 4s）。

边界：它只保证"我们写入的那个剪贴板后端里是正确的文字"，**不能**保证目标应用看到的内容——Wayland 会话下 arboard 可能退回 XWayland 后端，而目标应用读原生 Wayland 剪贴板，那种不一致看不出来（那类环境本就不支持自动粘贴，回退手动 Ctrl+V）。

### 2.6 paste.rs — 按键模拟

| 场景 | 快捷键 |
|---|---|
| macOS | `Cmd+V`（终端同样是 Cmd+V） |
| Linux 终端窗口 | `Ctrl+Shift+V` |
| Linux 其他 / Windows | `Ctrl+V` |
| 自动发送 | Enter（`Key::Unicode('\r')`） |

- 终端识别（仅 Linux）：用 `x11rb` 读 X 的 `GetInputFocus`，再沿窗口树向上最多 20 层找 `WM_CLASS`（`res_name`/`res_class`）。命中判定 = 类名包含 `terminal`，或落在内置清单里（`xterm`、`kitty`、`alacritty`、`ghostty`、`wezterm`、`konsole`、`foot`、`cool-retro-term` 等异类终端）。任何检测失败都**安全回退 Ctrl+V**。
- **同一条连接按序注入**：`simulate_paste()` 返回自己的 enigo 实例，自动发送复用它。同连接内的请求由 X server 按序处理，粘贴与回车不靠墙钟时间排序。
- **注入回车前复核焦点**：`focused_window_id()` 在注入前与发回车前各取一次（Linux/X11，取带 `WM_CLASS` 的客户端窗口而非内部子窗口），变了就跳过回车。非 Linux 或检测不到时返回 `None`，按"无从判断"处理（fail-open：宁可漏一次检查，也不少发一条消息）。
- **两段等待都是安全余量，不是性能参数**：写剪贴板后等 50ms（正确性由同步所有权保证，见 10.1，这 50ms 只是给目标应用的剪贴板缓存留余量）；回车前等 `250ms + 每 200 字符 1ms`（上限 +500ms）——人对这个量级无感知，而漏发的代价是消息没出去。**不按机器、不按应用做特调**（见第 10 节）。
- Wayland 判定：`XDG_SESSION_TYPE=wayland`，或（设置了 `WAYLAND_DISPLAY` 且没有 `DISPLAY`）→ 自动粘贴/自动发送判定为不支持，回退纯剪贴板。有 `DISPLAY` 时走 XWayland，XTest 仍可用。

### 2.7 discovery.rs — 凭证门控的 UDP 发现

请求（手机广播，3 轮）：

```json
{"type":"vl_disc_req","protocol":1,"client_id":"<手机 uuid>","proof":"<SHA-256(token) hex>"}
```

PC 端处理规则：

1. 不是 `vl_disc_req`、协议版本不是 1、`client_id`/`proof` 为空 → 静默丢弃。
2. 凭证命中以下**任一**才继续，否则静默丢弃（**电脑对陌生设备隐身，不产生任何应答**）：
   - 常用设备的长期 token：`(client_id, proof)` 与 `primary` 记录匹配；
   - **窗口期内仍有效的临时续连凭证**：`(client_id, proof)` 与内存中的续连记录匹配
     （只读校验、不续期；主人一旦断开、手机 bye、TTL 到期即失效）。
3. 命中 → 仅对请求来源**单播**：

```json
{"type":"vl_disc_resp","protocol":1,"device_id":"<pc uuid>","device_name":"hejiong-PC",
 "device_type":"pc","port":8765,"busy":false,"capabilities":{"auto_paste":true}}
```

`busy` 只给布尔值，不泄露占用者身份。用 `SHA-256(token)` 而非 token 本身作 proof：抓到广播不足以通过 TCP 认证。发现线程与 TCP 服务同端口，绑定失败只记日志，不影响 TCP 主服务。

**为什么也认临时续连凭证**：临时设备（断线续连窗口内）只记得 IP+端口，电脑换了 IP 就再也
找不到——它连"常用设备"的持久凭证都没有，广播是唯一出路。放宽到"窗口期内的临时访客"后，
临时设备与常用设备在"换 IP"这件事上体验一致（FR-15 的公平原则）。代价与边界写在
SECURITY.md，并在 §8 有对应测试（有效凭证应答、错误凭证与已作废凭证仍然静默）。

### 2.8 lib.rs — 装配、托盘与 commands

启动流程：初始化日志 → 加载配置 → `regenerate_code()`（**每次启动都换新码**）→ 按配置同步系统自启状态 → 建 `ServerState` → 建托盘 → spawn TCP 服务线程与 UDP 发现线程。

托盘菜单（GTK 下状态行必须保持 enabled，否则整项连 emoji 一起被渲染成灰色，点击由兜底分支吞掉）：

```
⚪ 等待配对 | 🟢 <手机名>
（只显示已连接手机名，**不标注身份**——身份词已全部退出界面）
打开主窗口              ← 落到主页，滚动位置归零
设置                    ← 打开窗口并直接切到设置页（发 view-request = "settings"）
断开当前手机            ← 无会话时禁用
──────
重启应用                ← 装好新版本后一键切换，见下
退出
```

托盘图标左键单击也会唤起主窗口（等同「打开主窗口」）。窗口关闭按钮只是隐藏（`prevent_close` + `hide`），真正退出走托盘「退出」。

主窗口（380×520，可缩放，最小 340×120，启动即可见）：

- 前端每 1s 轮询 `get_pairing_info`；轮询顺带调用 `ensure_code_fresh()`——只要没连接、且配对码已过期或剩余不足 2 分钟就自动换新，保证窗口上展示的二维码永远可用（PC 长期运行时不会变成死码）。
- 配对成功后先通知前端渲染绿色「已连接」页，0.9s 后自动隐藏到托盘；是否收回由设置项「连接后自动隐藏窗口」（`auto_hide`，默认开）决定。历史上这里用的是"窗口是否被用户手动打开"的隐形标记（`manual_window`），它会因为"为了扫码从托盘打开一次窗口"而永久禁用自动收回——v0.2.4 换成了显式设置项，该标记已删除。
- QR 内容格式 `beeptype://<ip>:<port>?code=<code>`，由 `qrcode` 渲染成 SVG 内联返回，无需前端构建链。
- TCP 绑定 `0.0.0.0:<port>`；`lan_ip()` 仅是取默认路由网卡的 IPv4（`UdpSocket::connect` 只做路由选择，**不发包**）用于生成二维码里的地址。

Tauri commands（13 个）：

| command | 作用 |
|---|---|
| `get_status` | 粗粒度状态文本（等待配对/已连接） |
| `get_pairing_info` | 二维码内容 + SVG + 常用设备席位 + 连接摘要（顺带续期配对码） |
| `get_settings` | 自动粘贴/自动发送的实际生效值、平台支持情况、开机自启、常用设备名、版本号 |
| `set_auto_paste` | 存用户意愿；运行态存「意愿 && 平台支持」；关闭时联动关闭 auto_send |
| `set_auto_send` | 存用户意愿；运行态存「意愿 && auto_paste 生效 && 平台支持」 |
| `set_auto_hide` | 「连接后自动隐藏窗口」开关（纯 PC 端行为，无平台能力判断） |
| `set_resume_ttl` | 临时设备续连窗口（分钟，0=关闭）；改完立即生效（窗口期在每次活动时读取） |
| `promote_session` | 把当前在线的**临时**手机设为常用设备（仅还没有常用设备时；凭证推送失败会回滚绑定） |
| `set_autostart` | 操作系统自启项 + 持久化 |
| `set_remember_next` | 「记住这台手机」勾选（仅对下一次码配对生效） |
| `disconnect_session` | 托盘/窗口断开当前手机 |
| `unbind_primary` | 解绑常用设备（在线会话降级为临时，不断开） |
| `set_window_height` | 前端启动时量一次真实高度后固定窗口高度 |

### 2.9 前端（desktop/src/index.html）

- 主页与设置页**绝对定位叠放**，切换只改 `opacity`/`visibility`（交叉淡化），元素几何全程不变——规避 WebKitGTK 层重建导致的标题闪烁。
- 窗口高度在启动时用离屏克隆渲染「等待态」量出真实高度（两种勾选文案取高者）后固定，之后不再随状态跳变：主页内容裁切、设置页内部滚动。
- 渲染做了结构键缓存（`lastStatusKey`/`lastPairKey`/`lastDeviceKey`），避免 1s 轮询全量重绘导致 hover/勾选态丢失；只有「无连接 → 有连接」这类跨状态切换才播入场动画。轮询同时刷新设置页的「常用设备」卡（手机断开/转正后卡片立刻纠正，不会停在已被踢下线的旧卡片上、点下去才报错）。
- **每次唤起窗口都是"重新进入"**：托盘「打开主窗口」/「设置」带 payload 发 `view-request`，前端据此落到对应页面，并在 `showView()` 里把目标视图的 `scrollTop` 归零——不沿用上一次停留在哪一页、滚到哪里（`showView` 的"已在该视图"提前返回之前就要重置，否则托盘重复唤起会留在上次的滚动位置）。
**重启应用**（托盘菜单 →「重启应用」）：用于"装好新版本后一键切换"。两个要点：

1. **必须延迟启动**：单实例插件在检测到已有实例时会直接 `exit(0)`。若新旧进程重叠，新进程会自杀、旧进程也在退出——应用凭空消失。因此先起一个"等 1 秒再 exec"的启动器（unix：`sh -c 'sleep 1; exec "$0"' <exe>`，Windows 经环境变量交给 `cmd`），**确认 spawn 成功后才退出自己**；失败则返回错误、留在界面提示。
2. **路径在启动时捕获**（`exe_path`，setup 里写入）：装新版本后 `/proc/self/exe` 会变成 "… (deleted)"，运行时再取就找不到可执行文件；启动时捕获则正好指向**新装**的那个二进制。

- 前端监听两个托盘事件：`view-request`（payload = `"home"`/`"settings"`，唤起窗口后落到指定页面 + 立即刷新一次，不等 1s 轮询）与 `pairing-show`（后端状态变化——配对成功、自动隐藏前——让前端立刻重绘）。
- 设置失败一类提示走**页内浮动提示**（`.notice`，`textContent` 写入、4 秒自隐），不用 `alert()`：系统弹窗会带出页面 URL/标题等实现细节，而且要用户点掉才继续。错误文案经 `errText()` 剥掉 `Error:` 这类实现前缀。
- 设置页每次进入都拉一次 `get_settings`；分区顺序是**定死的规则**：**粘贴与发送 → 连接行为 → 启动 → 手机绑定 → 关于**。新增设置项按这条线归位，不要再"就近插入"——之前"临时设备断线后自动续连"被插到开机自启前面，就是随手追加造成的排序漂移（用户反馈过）。
  - 粘贴与发送：接收后自动粘贴 / 粘贴后自动发送
  - 连接行为：连接后自动隐藏窗口 / 临时连接保活
  - 启动：开机自动启动
  - 手机绑定：已绑定手机卡片（解除绑定）／**绑定这台手机**／空态引导
  - 关于：版本（重启入口在托盘菜单，不放在设置页）；「记住这台手机」勾选框仅在还没绑定手机时出现，默认不勾（扫码默认即临时接入）。

---

## 3. 通信协议

### 3.1 传输与编码

TCP 长连接，UTF-8，每条消息一行 JSON 以 `\n` 结尾，单行上限 256KB。PC 端对超长行**直接断开连接**（不截断）。

### 3.2 消息字段

#### hello（手机 → PC）

```json
{"type":"hello","code":"<64 hex>","client_id":"<uuid>","device_name":"Xiaomi 14","hardware_id":"<android_id>"}
{"type":"hello","token":"<64 hex>","client_id":"<uuid>","device_name":"Xiaomi 14","hardware_id":"<android_id>"}
{"type":"hello","code":"<64 hex>","client_id":"<uuid>","device_name":"Xiaomi 14","hardware_id":"<android_id>","has_local_credential":false}
```

`code` 与 `token` 二选一；`client_id`/`device_name`/`hardware_id` 均可缺省（旧客户端兼容）。

`has_local_credential`（可选，只有扫码路径会带）：手机本地**是否还存着长期凭证**。`false` = 重装 / 清过数据。缺省（旧手机不发）按"有凭证"处理，行为不变。

#### set_prefs（手机 → PC）

```json
{"type":"set_prefs","auto_paste":true,"auto_send":true}
{"type":"set_prefs","auto_paste":false,"auto_send":false}
```

改电脑端的行为开关。**两个字段都必填、整对下发**：这两个开关在语义上是一对（自动发送依赖自动粘贴），只发被改动的那个会留下"依赖不一致"的窗口——所以手机端先用 `prefsAfterToggle()` 算出目标的一整对再发（关粘贴时直接发 `{false,false}`）；电脑端仍保留防御性联动（万一收到非法组合也存成合法状态）。

**只接受常用设备席位**：这两个开关改的是电脑的全局行为，临时访客没有权限——电脑端收到不合规请求会忽略，并照样回推一条 `prefs` 让手机自行纠正界面（不做静默失败）。落盘走 `ServerState::apply_prefs()`，与电脑端设置页的 `set_auto_paste` / `set_auto_send` 是同一条路径（依赖关系也只写一处：**自动粘贴关 → 自动发送同时关**），改完同时 **`emit("settings-changed")`** 通知电脑端自己的设置页刷新那两个开关。

#### prefs（PC → 手机）

```json
{"type":"prefs","auto_paste":true,"auto_send":false}
```

当前行为开关（用户意愿值）。三个时刻会推：① 手机改完（唯一一条回执）；② 电脑端设置页改完；③ 手机刚连上（席位建立后）——保证两端显示一致。临时席位不推（手机端在临时身份下根本不显示这两个开关）。

#### hello_ack（PC → 手机）

| 字段 | 类型 | 说明 |
|---|---|---|
| `ok` | bool | 是否成功 |
| `status` | string | `ok` / `occupied` / `bad_code` / `unauthorized` / `cooldown` |
| `error` | string? | 人话错误描述（如「尝试过于频繁，请 60 秒后再试」） |
| `session` | string? | `primary` / `transient`（仅成功时出现） |
| `device_id` / `device_name` / `device_type` | string? | PC 身份，`device_type` 固定 `pc` |
| `token` | string? | **仅新绑定常用设备那一次**出现，明文只下发一次 |
| `capabilities` | object? | `{"auto_paste": bool}` |

#### text（手机 → PC）

```json
{"type":"text","id":"<消息 id>","text":"你好world🌍","ts":1730000000000}
```

#### ack（PC → 手机）

```json
{"type":"ack","id":"<对应 text 的 id>","ok":true,"pasted":true,"srv_recv_ms":1730000000123}
```

| `code` 取值 | 含义 |
|---|---|
| `not_connected` | 连接未持席（未认证/等待接管/被顶替） |
| `empty_text` | 文本为空，未动剪贴板 |
| `clipboard_error` | 剪贴板写入失败（同时弹系统通知） |

#### 其他

| 消息 | 方向 | 说明 |
|---|---|---|
| `ping` / `pong` | 手机 → PC / PC → 手机 | `ping` 无字段；`pong` 可带 `session: "primary" \| "transient"`（**权威身份**）与 `resume_expires_in`（临时接入的**窗口剩余时长**）。两项都只对真正持席的连接给出；缺省表示"不给声明"，手机忽略。手机据此自愈身份、并把本地到期时间同步成 PC 的权威值 |
| `{"type":"ping"}` / `{"type":"pong"}` | 手机 ↔ PC | 心跳，任何连接都可发 |
| `{"type":"takeover"}` / `{"type":"takeover_ack","ok":bool}` | 手机 → PC → 手机 | 常用设备接管被占用的席位 |
| `{"type":"bye"}` / `{"type":"bye_ack"}` | 手机 → PC → 手机 | 优雅断开，PC 随即释放席位 |
| `{"type":"session_ended","reason":"..."}` | PC → 手机 | 被踢前通知；`owner_takeover` / `owner_disconnect` / `replaced` |
| `{"type":"session_downgraded"}` | PC → 手机 | 常用设备被解绑，当前会话降级为临时（连接保持）；可携带补发的续连凭证 |
| `{"type":"promoted","token":"…","device_id":"…","device_name":"…","device_type":"pc","capabilities":{…}}` | PC → 手机 | 在设置页把这台临时手机设为常用设备：长期凭证明文仅此一次下发 |
| `{"type":"promoted_ack"}` | 手机 → PC | 已保存长期凭证（PC 只记日志） |

### 3.3 容错与兼容

- 非法 JSON / 未知 type：只记日志、**不断连接**（`warn` 后可继续收消息）。
- 首条消息不是有效 `hello`：后续 `text` 一律回 `not_connected`。
- `hello_ack` 的所有新字段都有 serde 默认值，旧 PC 发的 `{"ok":true,"error":null}` 也能解析；旧手机缺 `client_id`/`hardware_id` 同样能解析。

---

## 4. 手机端（Flutter）

### 4.1 工程结构

```
app/lib/
├── main.dart                      # 入口：初始化存储后再 runApp（冷启动 autoConnect 依赖它）
├── pages/
│   ├── home_page.dart             # 主界面：状态胶囊 + 输入卡 + 占用页 + 引导页
│   ├── scan_page.dart             # 扫码页（mobile_scanner）
│   └── settings_page.dart         # 设置页：「电脑端设置」（行为开关）+ APP 信息（图标/名称/slogan/版本/版权）
└── services/
    ├── tcp_service.dart           # TCP、分帧、请求/应答 Completer、心跳
    ├── text_sender.dart           # 编排：自动连接、配对、降级、发送
    ├── storage_service.dart       # client_id、已绑定电脑列表、设备名
    ├── discovery_service.dart     # UDP 广播发现
    └── app_info.dart             # 读安装包版本号（走平台通道）

app/assets/app_icon.png            # 关于页的品牌图标，由 scripts/build_icons.py 从母版生成
```

平台通道 `beeptype/device`：Android 在 `MainActivity.kt` 实现 `getDeviceName`（按品牌优先级取系统设备名）与 `getHardwareId`（ANDROID_ID）；iOS 在 `AppDelegate.swift` 实现（`UIDevice.current.name` 与 Keychain 持久化 UUID）。关于页的版本号也走这条通道（`getAppVersion`：Android 读 `versionName`、iOS 读 `CFBundleShortVersionString`），**不引入 `package_info_plus`**——能少一个依赖就少一个，而这个值本来就来自平台。

### 4.2 依赖

| package | 用途 |
|---|---|
| mobile_scanner | 二维码扫描 |
| shared_preferences | 身份与已绑定电脑持久化 |
| network_info_plus | 取 WiFi IP 以推算定向广播地址 |
| crypto | 计算 `SHA-256(token)` 作为发现 proof |
| `dart:io` Socket / RawDatagramSocket | TCP 与 UDP（标准库） |

`connectivity_plus`：**网络变化触发器**（换 Wi-Fi / 切回原网络 / 开关移动数据时立刻重试一次，不干等退避周期）。连接状态的判定仍不依赖它（靠 socket 事件与心跳）。

`lan_check.dart` 只用 `dart:io` 的 `NetworkInterface.list()` 读本机网卡地址来推断"是否可能不在同一局域网"——**不需要任何权限**（读 Wi-Fi 名称才需要位置权限，所以不用）。

### 4.3 tcp_service.dart

- 分帧：把收到的字节拼进 `StringBuffer`，按 `\n` 切行，余下部分留作下一批。
- 请求/应答配对：`hello`、`takeover`、`bye`、`text` 各自用 Completer 等待对应的 `hello_ack` / `takeover_ack` / `bye_ack` / `ack`（按 `id` 索引）。
- 超时：`hello` 默认 10s、`takeover` 8s、`text` ack 5s、`bye` 等 `bye_ack` 兜底 1.5s；任何超时都会按失败结果返回并清理资源，不抛给 UI。
- 心跳：每 2s 发一次 `ping`（判活窗口 6s = 3 次心跳；PC 端 15s 收尸，切后台被 Doze 延迟也有余量）。
- **判活超时（6 秒，心跳 2 秒）**：这么久没收到**任何**服务端消息就判定连接已死，走与 socket 报错相同的断开路径——最慢 8 秒内界面就从绿点变成灰点并开始重试。必须有它的原因：对端只是断网（关 Wi-Fi、拔网线、路由器重启）时 TCP 不会报错——没有 FIN 也没有 RST，这条连接在手机上"看起来一切正常"，只发不问的心跳会让界面一直显示绿灯、既不重连也感知不到（用户实测：只有重开 APP 才会连）。
  - **只在前后台判定**，且切回前台重置判活基准：后台/Doze 期间定时器被系统延迟，拿休眠前的时刻判定会把健康连接误判成已死。判定本身抽成纯函数 `livenessExpired()`，有单测。
- **断开来源只用于判定归属，不出现在界面上**：`lastDisconnectWasPeer`（收到 FIN）**不是**无条件等于"电脑退出了"——锁屏/切后台时本进程定时器被挂起，我们不发心跳，PC 端 15 秒超时也会关连接，那条 FIN 的责任在手机侧。因此判定要过两道护栏（`peerClosedWhileTalking` + 网络是否刚变过，见 §4.7 的 `peerCloseIsReceiverSide()`）。界面上一律同一个表现（灰点 + 「连接」）。
- 状态机：`disconnected → connecting → (pairing) → paired`，另有 `occupied`（已认证但席位被占，连接保持）与 `failed`。
- **主动探活 `verifyAlive()`**：发一个 ping，等**任何**服务端消息回来（默认 1.2 秒）。用在"回到前台"——判活只在前后台做，后台期间连接断了本地并不知道，界面可能一直挂着"已连接"；探一下才知道该不该重连。活着就只唤起键盘（省掉一次无谓重连），死了就按正常断线走一轮重连。
- `session_ended` 与 `session_downgraded` 分别以 reason 字符串（含 `downgraded`）推给 UI 层。

### 4.4 text_sender.dart — 编排层

本轮加的三件事（都在这一层）：

- **身份自愈**：订阅 `tcp.authoritativeIdentityStream`（心跳里的权威身份）。电脑说"你是临时设备"而本机以为是常用设备 → 走与收到降级推送完全一样的处理（删本地凭证 + 会话标记为临时 + 通知界面刷新）。
- **续连兜底**：临时设备续连时，直连失败就广播找一次（发现服务认得窗口期内的续连凭证），找到新 IP 再连——临时设备因此也能挺过电脑换 IP。
- **旧记录不再挡路**：常用记录网络层不可达、广播也没人应时，若还留着一条**同一台电脑**的有效临时续连记录，就改走它（"解绑后用临时设备身份重连过"就会留下这种组合，旧实现会一直死磕失效的常用记录）。

凭证是否该删只认"电脑**明确拒绝**"（`unauthorized`）：被限流（`cooldown`）不算——那只是
"尝试太频繁，等 60 秒再来"，删绑定等于"一次限流换一台好设备重扫"。

`autoConnect()`（打开 APP 的默认路径）：

1. 取最近使用的一台已绑定电脑（`last_connected_ms` 最大者）。
2. **并行**：直接连缓存 IP（TCP 连接超时 2.5s，`hello` 5s）＋ UDP 发现（约 1.1s 出结果）。
3. 直连结论明确（成功 / occupied / 凭证被拒）就结束，不再换 IP 试。
4. 直连网络层失败才用发现结果补连一次（DHCP 换 IP 的主路径），连上后刷新缓存里的 IP。
5. 凭证被拒（`unauthorized`）→ 由上层删除本地凭证并停止重连。

其他入口：`pairWithCode()`（扫码配对，只有拿到 token 才落盘）、`pairFromQr()`（解析 `beeptype://` URI）、`takeover()`、`userDisconnect()`（手动断开并标记本进程不再自动重连）、`downgradeToTemp()`（PC 解绑后删本地凭证、会话转临时，不断开）、`forgetDevice()`。

`manualDisconnected` 是**内存态**标记：用户亲手点断开、或 PC 主动断开（`owner_disconnect`）后本进程内不再自动重连，冷启动即失效。

### 4.5 storage_service.dart

| 键 | 内容 |
|---|---|
| `client_id` | 本机 UUIDv4（`Random.secure()`） |
| `output_devices` | JSON 数组：`{device_id, name, device_type, ip, port, token, capabilities, last_connected_ms}` |
| `device_name` | 手机展示名（每次启动与切回前台从平台通道同步真实系统名） |

旧版键（`pc_ip`/`pc_port`/`pairing_code`/`is_paired`）在初始化时一次性清除（旧配对码无法兑换新 token，必须重新绑定）。**临时会话不写任何键**。数据结构支持多台电脑，v1 只自动连最近使用的一台。

### 4.6 discovery_service.dart

3 轮广播（间隔 300ms）＋ 最后一轮后 500ms 收尾，总预算约 1.1s；广播地址同时用有限广播 `255.255.255.255` 与按 WiFi IP 推算的子网广播（部分 ROM 会丢弃前者，只对常见 /24 私网段推算）。按 `device_id` 去重收集应答，`ip` 以**包来源地址**为准（NAT/多网卡下最可靠）。

### 4.7 页面与状态

**打字卡（输入区）的三个交互**（都为用户实测后定稿）：

- **断开时整张卡可点** = 按「连接」（被占用 + 常用设备 = 接管）；判定抽成纯函数 `editorTapIntent()`——**正在连接/重连中点了不生效**（那一轮已经在试，再触发只会重置倒计时、还可能和轮内在飞的尝试打架；要停手用胶囊的「中断」）。实现上未连接时用 `AbsorbPointer` 挡住禁用输入框，让点击落到卡片本身。
- **底部只留兜底发送按钮**：行为开关已挪进设置页（见下）。
- **焦点与键盘**：连上、回到前台、以及**从设置页/扫码页返回且已连接**时，都走 `_requestKeyboard()`（聚焦 + 弹键盘，缺一不可——没有焦点语音的字进不了打字区）。
- **断开态那句提示**：「点此区域连接电脑，连上后长按键盘语音键语音输入文字」——把"这里可以点"和"连上后怎么说"合成一句；**不再提胶囊上的连接按钮**（它本身就在那儿、一眼可见）。已连接时仍是「长按键盘语音键即可语音输入文字」；"不在同一网络""被占用"两格保持原样（它们本身就是可执行的建议，不该被通用文案盖掉）。

**设置页的刷新方式**：电脑端自己的设置页靠**事件推送**（`settings-changed`，与 `view-request` / `pairing-show` 同一套机制）即时更新那两个开关，不再等 1 秒轮询；轮询仍负责主页状态、配对信息与「手机绑定」卡。彻底事件化（含二维码的时间驱动部分改用本地计时器）列为后续可选优化。

`home_page.dart` 的 UI 骨架不随连接状态跳变，只有顶部状态胶囊变色与标签、主体区换视图：

| 时刻 | 状态点 | 电脑名 | 词 | 按钮 | 打字区（只讲「该做什么」） |
|---|---|---|---|---|---|
| 正在连接 / 重试中 | 柔和呼吸 | 名字 | **连接中 · N 秒**（整轮 30 秒倒计时） | **中断** | 连接成功后即可语音输入文字（此时点打字区不生效，故不写「点此区域连接」） |
| 已连接 | 绿常亮 | 名字 | — | 断开 | 长按键盘语音键即可语音输入文字 |
| 连不上（电脑没开/退出/断网/不在同一网络） | 灰常亮 | 名字 | — | 连接 | 默认同第一条；**不在同一局域网**时换成「请确认手机和电脑连着同一个 Wi-Fi」 |
| 被占用 · 本机是绑定手机 | 灰常亮 | 名字 | — | 连接（点它即接管） | 「另一台手机正在使用这台电脑；点「连接」即可立即接管」 |
| 被占用 · 本机是临时接入 | 灰常亮 | 名字 | — | 不显示 | 无提示：清临时记录、回扫码引导页 |
| 用户主动断开 | 灰常亮 | 名字 | — | 连接 | 点此区域连接电脑，连上后长按键盘语音键语音输入文字 |
| 没绑定过电脑 | 灰常亮 | 未连接电脑 | — | 不显示 | 正文＝扫码引导页 |

**点色与呼吸**：绿常亮 = 已连接；灰常亮 = 未连接（含被占用、主动断开）；**柔和呼吸**（透明度 0.35↔1.0、约 1.2 秒一个来回、缓入缓出）= 正在尝试（含重试中）。系统开启「减少动态效果」时呼吸退化为灰常亮——宁可少一个提示，也不硬闪。

**电脑名后面的身份标注**：只有**临时连接**标两个字「临时」；常用连接是常态、不说话。四条边界：① 重连中/连接中不标（那个位置被「连接中 · N 秒」占着，两个词会打架）；② 主动断开（含电脑端断开）一律不标；③ 已连接/被占用以**会话身份**为准（心跳会纠正身份，它最权威）；④ 其余状态以**本地记录**为准（有绑定电脑 = 常用；只剩临时记录 = 临时）。这条标注同时承担"通知"作用：电脑端解绑、或身份被纠正时胶囊立刻显示「临时」，不需要另加提示机制。

**临时接入的断开处置**：只在"**确定是电脑侧关闭了连接**"（收到 FIN、且当时我们还在正常通信、网络也没刚变过，`peerCloseIsReceiverSide()` 纯函数有单测）时才立刻清记录回扫码页。其余情况（静默断、手机自己换网、还在重试中）一律**保留记录、给机会**——按产品认知："电脑程序还在正常跑"就该让它在窗口期内自己连回。

**心跳同步窗口**：PC 的窗口每次心跳都在滑动，而手机本地的到期时间只在"配对/续连成功"时刷新——所以 `pong` 会带上 `resume_expires_in`，手机收到就 `touchTransientSession()`（节流 30 秒一次）。否则连接正常用了超过窗口时长后，手机会提前自以为过期。

临时记录到期时另排一个一次性刷新（`_scheduleTransientExpiryRefresh()`）：手机端界面是事件驱动的，没有它的话，窗口过期后界面会停在"未连接"的输入卡上直到用户碰它。

**被占用时的两条分岔**（`AuthMode`）：用长期凭证连的（本机是这台电脑的绑定手机）→ **立即停止自动重连**，打字区给接管提示，等用户点「连接」（点它发 `takeover`）；用扫码/临时凭证连的 → 属于正常竞争，清掉临时续连记录回扫码引导页（电脑此刻不显示二维码，本身就说明它正忙）。

**三个面的分工**（新增 UI 文案时按这条走，别再让两处说同一件事）：

- **状态胶囊 = 只讲事实**：现在是什么状态（已连接 / 常用设备 / 临时设备 / 正在连接 / 连接失败，重试中 / 另一台手机正在使用）。
- **打字区 = 只讲"该做什么"**：默认一句功能说明，有可执行建议时替换；**永不复述胶囊里的事实**。所以"两处都有话说"不构成冲突，而是各就各位。
- **一次性轻提示（snack）= 只讲"刚刚发生了什么"**：被解绑、临时会话过期这类事件——它们出现时界面往往已切走（没有打字区），只能靠一次性提示说明。

**"不在同一个局域网"怎么判断**（`lan_check.dart`，决定要不要给那句 Wi-Fi 建议）：
拿**目标电脑的地址**与手机当前所有 IPv4 地址按 /24 比——目标不在任何一个网段里，
才提示"请确认手机和电脑连着同一个 Wi-Fi"（手机用移动数据、连了另一个 Wi-Fi 都归这类）；
网段相同就**一句话都不说**。故意保守：私网网段到处复用（192.168.1.x 几乎是路由器默认值），
同网段既可能真的同网、也可能只是撞段；而且公共 Wi-Fi 常禁止设备互访——那种"同网也连不上"
不该甩锅给用户去查 Wi-Fi。读网卡地址不需要任何权限（读 Wi-Fi 名称才需要位置权限，所以不用）。

自动重连（`retry_episode.dart` 的状态机 + `retry_policy.dart` 的文案选择，都有单测）：**等间隔 2 秒、一轮 30 秒（墙上时钟）**，到点即停手。为什么不用指数退避：一轮预算固定，而每次尝试本身要花 1～4 秒（直连超时最多 2.5s + 广播约 1.1s）——退避的前段短等待被尝试耗时吃掉、后段 8/9 秒的长等待纯属白等；等间隔把预算铺匀，期望恢复延迟从约 3.3 秒降到约 1 秒，尝试次数还更多（约 6～9 次）。最后一次等待会压缩进剩余时间里。

**"一轮"是显式状态**（`RetryEpisode`，纯逻辑、8 条单测）。它存在的理由是两次真实事故：

1. **倒计时归零后重试还在跑**——一轮的结束不能用"试满 N 次"判断：每次尝试本身要花 1～4 秒，按次数算真实时长接近翻倍，倒计时先归零而重试还在继续（表现为"第二段只有「连接中」没有秒数"）。所以改成**墙上时钟**是权威，倒计时与它同一基准。
2. **一轮结束后又冒出一轮**——"是否开新的一轮"曾用"倒计时定时器不存在"来判断，而到点收掉倒计时正好让它变成不存在，于是那次还在飞的尝试收尾时又开了一轮、无限续命。所以改成：**一轮只能由明确的触发开启**（用户点「连接」/ 回到前台 / 网络变化 / 冷启动 / **刚断开这个状态跳变**），一轮内部的尝试失败只延续当前轮；结束一轮时同时**作废在飞的那次尝试**，界面立刻落到灰点 +「连接」。

**倒计时在开轮那一刻就定下截止、当场可见**（`start()` 里设 `_deadlineMs`）：不留"显示连接中却没有秒数"的空白。代价是"只有移动数据"这类一轮会在几秒内结束、倒计时亮一下就灭——这是**可接受的**，因为它表示"已经判断出到不了电脑"（用户拍板：两者相权，取前者）。胶囊的倒计时**在"第一次重试真的被排上"时才起**（不在首次尝试时起）：这样它承诺的就是真实剩余的等待时间——"只有移动数据、压根不重试"之类的场景不会出现一个马上消失的假倒计时（用户实测踩到过）。重试期间胶囊显示倒计时（`retrySecondsLeft()`，每秒刷新）并给「**中断**」按钮——用户看得见还要等多久，也能随时打断（停手 = 取消定时器 + 作废在飞尝试 + 置"本进程不再自动重试"，**续连记录不动**）。试满或停手后零定时器零广播，胶囊变灰点 +「连接」。

重新开始一轮的触发点（都表示"用户回来了 / 环境变了"）：**切回前台**、**网络变化**（`connectivity_plus`）、点「重新连接」、重新扫码。切后台不再无限重试——慢速长期重试并不"及时"（用户不如当场点一下），在移动数据上还会反复把无线电拉回高功耗。

**只有移动数据时不重试、不广播**：`lan_check.dart` 枚举网卡，若只有运营商接口（`rmnet*`/`ccmni*`…）就判定"没有局域网"，直接停手（Wi-Fi 回来由网络变化监听拉起）；此时打字区提示「请确认手机和电脑连着同一个 Wi-Fi」。网卡名认不出来时按"有局域网"处理——宁可多试一轮，也不要因为认不出名字而真的连不回来。

**重试怎么分流**（`discoveryOnly`）：手机每次重试前，用"电脑那个地址在不在我某个网段里"（`looksOffLan`）决定动作——

| 手机当前网络 | 动作 |
|---|---|
| 有局域网，电脑地址就在我的网段里 | 直连 → 失败广播 → 正常退避重试（电脑掉线/重启的主路径） |
| 有局域网，但电脑地址不在我任何网段里 | **跳过直连、只做广播**（电脑可能搬到我这个网络了、地址也变了）；一轮约 30 秒，每轮一次广播，找不到就停下 |
| 没有局域网（只有移动数据或热点都没有） | 不重试、不广播（广播出了这个网络也没意义），给提示，等网络变化 |

**广播预算与抗丢包**：一轮广播现在是"多而密"（5 轮 × 120ms + 收尾 400ms ≈ 1 秒）——UDP 广播本身会丢包（AP 限速、省电、信道忙），总时长不变但单次被全丢的概率大幅下降（用户实测"PC 换新 IP 后要将近一分钟才连回"，就是单轮广播被丢、要等下一轮才成功）。整段由 `discover()` 内部兜底；调用方只留一个 5 秒的保险丝，**不能**用更紧的超时去掐它——曾经外层是 2 秒，而广播前还有一步"查询 Wi-Fi 客户端地址"最多白等 1 秒，导致真正的广播常常还没发完就被掐断（PC 换 IP 后连不回来的一个根源）。那步多余的查询已删除：枚举本机所有网卡已经覆盖 Wi-Fi 与热点。

**广播应答要验明身份**：`discover(expectDeviceId:)` 只接受 `device_id` 等于记忆里那台电脑的应答。应答方虽然必须认得凭证才"应该"应答，但陌生设备可以盲答（见广播就回自己的地址），那会把手机引到错地址、甚至把凭证交给它。

**拒绝也要验明来源**：电脑在拒绝类 `hello_ack` 里带上自己的 `device_id`；手机核对"是**我那台**电脑拒的我"（`rejectionFromOurPc()`）才删除本地凭证——否则一台凑巧占了旧地址的陌生设备就能让好手机丢掉绑定。

**广播目标同样按"所有网卡"枚举**（`LocalNet.broadcastTargets()`）：每个局域网网段各发一份定向广播。只看"Wi-Fi 客户端地址"是不行的——手机开热点时它作为网关（ap0/swlan0）并没有作为客户端连着 Wi-Fi，全局广播会从移动数据出去、到不了热点网段，于是"电脑已经连上手机热点，手机却找不到它"（用户实测复现）。没有局域网网卡时广播直接跳过，发现流程立即返回、不做无谓等待。

**广播改为按需**：只在直连失败之后才广播（原来每次尝试都并发先广播一遍）。正常重连（电脑还在、只是刚断）全程零广播，省掉每次约 1.4 秒的白等。

扫码页：

- **进页先停手**：进扫码页前先把后台重试停掉、并作废在飞的连接尝试（`abortConnectAttempts()` 自增一次代号，旧尝试在每一步都会检查代号后直接退出）。否则旧尝试的超时事件会把刚配对好的界面按回"连接中"，更糟的是它会继续走自己的"广播补连"、把新连接拆掉重连——用户实测："第一次扫码配对成功却一直停在连接中，第二次才正常"。
- **配对返回后以 TCP 层的真实状态同步一次界面**（`isPairedNow`），避免任何残留事件把绿色按回"连接中"。
- **扫描器常驻**：`MobileScannerController` 是页面的字段、控件始终挂着，不随状态拆装。拆一次就等于重新初始化相机，失败后往往回不来——用户只能退出页面重进（"不能持续扫描"的真身）。
- **去重由我们自己管**（`_lastAttemptedRaw` + 同码最多 2 次）：避免取景框不挪开时连发请求，同时让"把镜头移到新码上"能自动继续；插件的 `detectionSpeed` 用 `normal`，把重试的开关留在我们手里。
- **全自动、无按钮、不设次数上限**：配对失败给提示，1.5 秒后**自动重新武装**（提示复位，同一张码也能再试）——扫码页上没有任何按钮，把镜头对准新码就自动继续。限流已经移除，所以反复请求也不会把人锁在门外。提示只在真正发起新一轮配对时被替换（那时显示「正在配对…」），不会出现"提示还挂着、其实什么都没发生"。
- **扫码框有扫动光带**（微信扫一扫那种观感）：`_ScanLine`，1.8 秒一个来回。
- **不打断已有连接**：扫到的是**当前已连着的那台电脑**（同 ip:port）时复用现有 socket，不为配对先把好连接拆掉；换到别的电脑才断开（一台手机同时只连一台电脑）。扫码页打开期间，首页**不做自动重连、不唤起键盘**（否则扫另一台电脑时会去重连旧电脑、甚至把键盘盖到扫码页上）；从扫码页返回而没配对成功时，再把之前那条连接找回来。
- **成功直接跳转**：不再先显示一句「连接成功！」再跳（跳转本身就是成功的反馈）。
- **单帧误读静默忽略**：读到的内容不是 `beeptype://…` 就丢帧继续扫（二维码每约 3 分钟自动换新、扫描瞬间也可能糊），只有电脑明确拒绝才给红字提示。
- **诊断日志**（定位"同一局域网内连不上"）：每次尝试打 `BT_CONN 尝试直连 <ip:port>` / `BT_CONN 直连结果：<status>` / `BT_CONN 广播找到 N 台：…` / `BT_CONN 用新地址补连 …`；广播前打 `BT_DISC 本机网卡：<名=地址…> → 广播目标：…`——这几行能直接回答"广播有没有发到电脑所在网段"。**所有 APP 日志统一用 `BT_` 前缀**（`BT_TCP` 会话/心跳、`BT_CONN` 连接流程、`BT_DISC` 发现、`BT_SCAN` 扫码、`BT_FOCUS` 焦点与键盘），正常路径不出声、只在异常与失败路径上打；`adb logcat | grep BT_` 即可捞出全部。

**设置页的两块**：上面「**电脑端设置**」= 两个行为开关（接收后自动粘贴 / 粘贴后自动发送，**文案与电脑端设置页逐字一致**，一眼看出这是电脑的设置；开关值与依赖说明都由电脑端推来），下面 APP 信息（**始终贴页面底部**：屏幕够高时布局用 spaceBetween 撑到整屏，内容超出才滚动，两种状态同一套布局）。显示条件抽成纯函数 `showPcSettings()`：**只在当前这条连接是常用设备身份、且已同步到值之后**出现——临时访客看不到（电脑端也不接受它的修改），没同步到值时也不显示（避免闪错值）。将来电脑端还有别的设置项都加进这一块。

**页面与入口（三个页面）**：主页、扫码页、设置页。设置页是二级页（`/settings`，带返回），从首页右上角的**齿轮一步进入**——**不做菜单中转**：今天页里只有一块内容（APP 信息），多一次点击没有意义。APP 目前没有设置项，将来有就加在这块信息**上面**（与 LocalSend 的设置页同一种排版：上面设置项、下面 APP 信息），入口与页面名字都不用改。信息块今天**不加小标题**——小标题的用途是分隔多组内容，只有一块时反而是多余的一层。

顶栏右上角平时是「扫码 + 齿轮」；**未连接且没有可连目标时隐藏扫码图标**——那个状态下正文本身就是扫码引导页、中间那个大按钮就是扫码入口，同一屏里出现两个一模一样的入口只会让人犹豫。

**「已连接」只有一个收尾入口**：把状态置为已连接必须走 `_markPaired()`（`_onStatus` 的 paired 分支也走它），它会一并结束重试那一轮、清掉临时到期刷新与上次失败留下的网络判断。**别直接给 `_status` 赋值**——绕过收尾会让界面停在"已连接 + 连接中 · N 秒 + 断开"（用户实测：连接状态里冒出倒计时，而语音发送完全正常）。配套两条不变量：① 从扫码页返回时要先判断"是否真的需要重连"，已连接/被占用/正在连接都不该开新的一轮；② 一轮的"到点"由 `RetryEpisode.isActive(now)` 自己算出来，**不依赖任何人调用 `shouldSchedule()`**——否则没人来问的那条路径会把「连接中」一直留在界面上。

### 4.8 语音发送时序（关键设计）

```
按住输入法麦克风 → 麦克风被占用（录音 = 真）
                  → 说话途中输入法可能**逐段提交**文本（手指没离开语音键也会提交）
松手 → 麦克风立刻释放
     → 输入法把最后一段提交完（实测最晚约 0.6 秒）
     → 发送输入框当前全文（合并窗口：语音 80ms）
普通键盘输入（无 composing） → 300ms 防抖后发送快照
```

**判据是"麦克风是否被占用"，不是 composing**：分段型输入法（讯飞/搜狗）会在说话途中逐段提交，
若按 composing 下降沿就发送，一整段会被打成碎片（真机日志里能直接看到连续三次误发）。
录音状态由 Android `AudioManager.getActiveRecordingConfigurations()` 经自建通道
（`getActiveRecordingCount`）每 **150ms** 查一次——实测按住语音键期间麦克风一直被占用（连停顿 5 秒也不放），
松手那一刻立刻释放。

松手后**该等的才等**：输入框里还有没提交的文字（composing 仍有效）⇒ 等它提交（这段等待省不掉，
否则会把完整的一段拆成两条发出去）；没有待提交的文字 ⇒ 立刻发。语音场景另加 **1200ms 兜底**
（输入法卡住不提交时把已有的发出去——宁可少最后半句，也不能让用户觉得"说了话什么都没发生"）。

**降级**：读不到麦克风状态（老系统把"谁在录音"匿名化、iOS 未实现）⇒ 整条机制不启用，退回
"composing 下降沿即发"的旧行为；那些机型上分段型输入法仍会被打成碎片（适配矩阵见 PRD NFR-19）。

判定抽成纯函数 `shouldFlush()`（`app/lib/services/voice_send_policy.dart`，9 条单测）。
发送成功后清空输入框；失败保留全文并提示，右侧有兜底发送按钮。

### 4.10 图片传输（分块 + 写剪贴板 + **不回车**）

目标：**手机选图 → 电脑剪贴板里就是这张图 → 在支持粘贴图片的输入框里自动贴出来**。
**只覆盖"支持图片粘贴"的输入框**；不支持的（上传框、纯文本框）没效果是可以接受的——
图片已经送到剪贴板，是那个框不收。**不做文件清单路线**（那条能一次粘多张到上传框，但必须落盘、
三平台各写一套）⇒ **全程内存、不落盘**。

**协议（三条，走现有 JSON 行通道）**：`image_begin{id,mime,total_bytes,chunks}` →
若干 `image_chunk{id,seq,data(base64)}` → `image_end{id}` → 复用 `ack{ok,code,pasted}`。
**为什么分块**：单行上限 256KB，且 base64 会让内存翻倍；每块原始 135KB（base64 后约 180KB）。
**能力协商**：电脑端在 `hello_ack.caps` 里加 `image`；**手机端只有见到这个位才显示图片入口** ⇒
旧电脑不认识新消息时手机上根本不显示，不需要为它做兼容逻辑。

**电脑端**（`image_receiver.rs` 纯状态机 + `server.rs` 接入）：
- 校验：MIME 白名单（jpeg/png）、单张 ≤8MB、块数 ≤200、`seq` 必须连续、累计字节不得超声明值、收齐后块数与字节数要和声明一致；30 秒没等到 `image_end` ⇒ 丢弃并回错误 ack；
- 解码：`image` crate → 尺寸上限 8000×8000（防解码炸弹）→ 转 RGBA → 剪贴板插件 `write_image`；
- 之后走**与文本相同的自动粘贴条件**（开关开 + 平台支持 + 我们自己窗口没焦点），
  但**刻意不自动回车**：贴完图通常还要配文字，自动回车会立刻把消息发出去。
- 与文本同理，图片消息只接受**席位持有者**。

**手机端**（`image_send.dart` 纯逻辑 + `home_page` 入口）：
- `image_picker` 选图（可多选），**统一带 `maxWidth: 2560, imageQuality: 92`** ⇒ 系统重编码成 JPEG，
  一举解决 ① HEIC/WebP 这类电脑端解不开的格式 ② 体积 ③ 8MB 上限几乎不触发；代价是截图也会被转 JPEG（q92 肉眼基本无损）；
- **先选后发**：选中的图片进入"待发区"（缩略图 + 逐张叉掉 + 「+」继续添加），**不立即发送**；
- **发送时机与文本一致**：点右下角「发送」键，或文字（含语音转文字）自动发送时一并发出；只有图片没文字时也能用手动发送；
- **先图后文**：图片先贴到光标处（不回车），文字随后贴并（必要时）回车 ⇒ 两者最终落在**同一条消息**里；
- 多张 ⇒ **串行逐张**发送；某张失败就停下并保留在待发区（提示"可再点发送重试"），不会静默丢。

**已知边界**：Wayland 等不支持自动粘贴的环境 ⇒ 图片也退化为"手动 Ctrl+V"；
图片同样会进目标机器的剪贴板历史（如实告知，不做承诺——"不留痕"那两条路已证走不通，见 §10.4）。

**真机自测**：手机端 `flutter test`（分块无损、单块 <256KB、上限、MIME）；电脑端 `cargo test`（`image_receiver` 的 8 条状态机单测）。

### 4.9 输入框聚焦与软键盘

连接成功后自动聚焦并强制唤起软键盘。冷启动存在竞态（Activity 尚未完成 IME 注册时 `TextInput.show` 会被系统取消），因此用 `viewInsets.bottom` 验证键盘是否真的弹出，未弹出则以 350ms 间隔重试，最多 3 次。切走方向（resumed → inactive）主动收起键盘，消除转场快照导致的键盘闪动。

---

## 5. 安全设计

### 5.1 凭证生命周期

```
启动：每次启动都 regenerate_code() → 新配对码（旧码立即失效）
展示：窗口轮询时若码已过期或剩余 <2 分钟则自动换新（未连接时才换）
配对：hello{code} → 校验通过 → 立即换新码（一次性）
        勾选「记住」且还没有常用设备 → 生成 token，明文随 hello_ack 下发一次
        常用设备席位已有主人 → 临时会话：PC 不落盘，但签发短期续连凭证（窗口期内可免扫码续连）
重连：hello{token} → SHA-256 比对命中 → 免码接入
转正：在设置页把在线的临时手机设为常用设备 → PC 生成 token 并推送 promoted
        （写入常用设备记录前先确认推送成功，失败回滚）→ 手机落盘为常用电脑
解绑：config.primary 清空 → 在线常用设备会话降级为临时（session_downgraded，不断开）→ 旧 token 立即失效
```

### 5.2 权限边界

- 绑定常用设备的唯一入口是 **PC 端窗口的勾选框**，协议里没有任何手机端可以修改信任关系的指令。
- 临时手机拿不到 token，也发不出有效 proof，设备发现对它不存在。
- 常用设备遇到席位被占只能等或接管，**绝不自动踢人**；临时方发 `takeover` 一律被拒。
- 能物理操作 PC 的人即可信（托盘/窗口操作）。

### 5.3 日志脱敏

只记元信息，永不记文本内容：

```rust
log::info!("recv id={} type={} len={}", id, msg_type, text.len());
log::info!("ack sent id={} ok={} non_text={}", id, ok, non_text);
```

### 5.4 实现与设计设想的差异（有意保留）

| 项 | v0.1 设想 | 当前实现 | 原因 |
|---|---|---|---|
| 配对码形态 | 6 位数字，可手输 | 64 位 hex 随机串，只随二维码下发 | 6 位码在局域网内可暴力枚举；取消手输入口后没有可读性需求 |
| 剪贴板非文本保护 | 检测到图片/文件则不覆盖 | 一律覆盖；仅空文本会被拒绝 | `arboard`/Tauri 插件不暴露「非文本 target」检测；家用场景价值有限 |
| 监听地址 | 仅绑局域网网卡 | `0.0.0.0`（TCP 与 UDP） | 多网卡/虚拟网卡环境（Docker、VPN）下判定「哪个是局域网网卡」易出错；改由高熵凭证承担 |
| 解绑行为 | 踢掉在线常用设备 | 降级为临时设备，不断开 | 用户解绑通常是想换机，打断正在进行的输入没有收益 |
| 模块划分 | 独立 clipboard/tray/qr 模块 | 并入 server.rs / lib.rs | 每个模块只有几十行，拆开后调用链更绕 |
| 心跳超时 | 手机 15s ping + 10s pong 超时 | 手机 5s ping；PC 15s 收尸；手机靠 socket 事件感知 | 少一个定时器，且避免 Doze 延迟导致误判断线 |

---

## 6. 平台差异

| 平台 | 剪贴板 | 粘贴快捷键 | 自动粘贴 | 备注 |
|---|---|---|---|---|
| Linux X11 | arboard（X11 直连） | 终端 `Ctrl+Shift+V`，其余 `Ctrl+V` | ✅ | 按键走 enigo（XTest），无需额外授权 |
| Linux Wayland | 同上 | — | ❌ | 安全模型禁止后台注入；且无差别写剪贴板也受限，回退手动 Ctrl+V |
| Windows 10+ | 原生 API | `Ctrl+V` | ✅ | SendInput；UIPI 下无法输入到更高权限窗口 |
| macOS 12+ | 原生 API | `Cmd+V` | ✅ | CGEvent；需要在系统设置里授予辅助功能权限 |
| Android / iOS | — | — | — | 手机端只做采集与发送 |

依赖都不需要用户安装外部命令（剪贴板走 arboard、按键走 enigo + X11 XTest），发行包不再依赖 `xdotool`/`xclip`。

**Linux 任务栏图标（winicon.rs）**：Tauri 的 `window.set_icon()` 在这套 GTK/tao 栈上**实测无效**——调用返回成功，但窗口的 `_NET_WM_ICON` 属性始终不写（立即调用与延后 2 秒调用都试过）。而缺该属性的窗口在 Deepin 的任务栏里会退回通用图标（现象：显示"两个齿轮"）；同机对照，Edge / ZCode 都设了这个属性、图标正常。因此改为自己实现：连 X server，从 `_NET_CLIENT_LIST` 里按 `_NET_WM_PID` == 本进程 pid 且 `WM_CLASS` 含 `beeptype` 找到自己的窗口，把 128×128 图标按 `[宽, 高, ARGB...]` 写入该属性（带重试，失败只记日志）。仅作用于 Linux/X11；Wayland 用 app_id 关联，Windows/macOS 用可执行文件图标。

**Android 启动器图标（自适应图标）**：画布 108dp，其中**只有中间 72dp 是可见区**——启动器会把这块放大到整个图标，外侧 18dp 只是视差/遮罩的溢出边。所以图形包围盒要按"可见区的比例"来定：`scripts/build_icons.py` 里的 `GLYPH_VISIBLE_RATIO = 0.67`，**顶满（1.0）就等于没有留白**（v0.5.0 之前就是这样，观感上比其它 APP 挤）。两条配套规则：① 抠图判据用**最亮通道**而不是"与底色的色差"——母版是 AI 生成的、底色带噪点，色差判据会把 82% 的底色噪点当成图形留下，留白一放大就是肉眼可见的方块痕迹；② 前景层按**图形包围盒**居中（早先是按含噪点的裁切框居中，图形实际偏左上约 2%）。legacy 图标（`ic_launcher.png` / `ic_launcher_round.png`，给 Android 8 以下）仍是满幅徽章、不缩，缩了在旧系统上会变成小方块加透明边。

---

## 7. 构建与发布

- **CI 分成两条**：`.github/workflows/ci.yml` 在 push 到 main / PR / 手动触发时跑轻量检查（`cargo test` + `flutter analyze` + `flutter test`，**不打包**，约 2~3 分钟）；`.github/workflows/release.yml` 只在推 `v*` tag（或手动 dry run）时做全平台打包与发布。日常迭代用本地构建的测试包（见 CONTRIBUTING 的「开发与发布流程」），**不每个改动都发版**。
- **发布（release.yml）**：仅在推 `v*` tag 或手动触发时运行。
  - `version` 从 tag 提取（`v0.2.0` → `0.2.0`），写入 `app/pubspec.yaml` 与 `desktop/src-tauri/tauri.conf.json`——**git tag 是版本唯一来源**。
  - **版本的三层关系**（别搞混）：tag → CI 改写 → ① `pubspec.yaml` 决定 Android 包版本，② `tauri.conf.json` 决定安装包版本**且被 tauri-codegen 烘焙进二进制**，所以设置页「关于」用 `app.package_info().version` 读的就是它（与安装包一致）；③ `Cargo.toml` 的 `version` 只是 crate 版本，**不参与展示**（曾经用 `env!("CARGO_PKG_VERSION")` 取它，导致发布包的关于页一直显示旧号）。仓库里的这三个字面量都只服务本地开发构建，保持与最近一次发布一致即可，CI 每次都会按 tag 覆盖前两个。
  - Android：`flutter build apk --release --target-platform android-arm64`，签名 keystore 由 Secrets 还原并生成 `key.properties`。
  - 桌面：矩阵 `ubuntu-22.04`（GLIBC 兼容性）/ `macos-latest` / `windows-latest`，`tauri build` 后分别产出 `.deb` + `.AppImage`、`.dmg`、`.msi`。
  - Release job 汇总所有产物，`workflow_dispatch` 触发时先建 draft。
- **本地构建**：桌面端 `cd desktop && cargo tauri build`（开发期 `cargo build` 后直接跑 `target/debug/beeptype`，前端是单文件 HTML，无需前端构建链）；手机端 `cd app && flutter build apk --release --target-platform android-arm64`。
- 仓库根的行尾与二进制属性由 `.gitattributes` 统一（`* text=auto eol=lf`）；`.trae/`、`target/`、签名文件在 `.gitignore` 中排除。

---

## 8. 测试与验证

**Rust 测试（37 个，`cargo test`）**

| 文件 | 覆盖 |
|---|---|
| `config.rs` | 配对码校验与过期、UUIDv4 形态、常用设备绑定/校验/解绑全生命周期、硬件指纹随常用设备记录持久化、落盘不含明文 token、旧配置迁移、`auto_hide` 缺省视为开启 |
| `protocol.rs` | 各消息 encode/decode 往返、旧客户端字段缺省兼容、畸形 JSON 不 panic |
| `server.rs` | 席位仲裁三分支（空席接入/同常用设备替换/被临时方占用）、踢人原因映射、QR 内容格式 |
| `discovery.rs` | 请求解析的合法/畸形样本、应答 JSON 字段 |
| `paste.rs` | 回车余量的边界（基准值、按长度递增、上限、整体 <1s） |
| `winicon.rs` | 图标数据格式（`[宽, 高, ARGB...]` 长度自洽、含不透明像素） |
| 续连相关 | 续连凭证校验（哈希/client_id/过期三者缺一不可）、窗口边界、协议往返（含 `resume` / `resume_expires_in`）、`resume_ttl_min` 默认值与旧配置缺省 |
| 设为常用相关 | 端到端集成测试（推送凭证 → 用该 token 换到 primary 席位）、无连接时拒绝、重启命令的形状（延迟启动 + 路径作为参数传入） |

**集成测试（9 个，`server::integration_tests`）**：用**真实 TCP 连接**扮演手机，驱动完整的握手/席位/凭证状态机——能挡住的正是"某条路径忘了做某事"这类单测覆盖不到的漏洞。

做法：`ServerState` 对运行时泛型化（`ServerState<R: Runtime>`，桌面固定用 `AppServerState = ServerState<Wry>`），测试里换用 tauri 的 `mock_app()`（MockRuntime）即可无头运行，因此 CI 上也能跑。覆盖：

- 解绑降级必须补发续连凭证且该凭证可用（用户实测发现的 bug，已验证"撤掉修复即失败"）；
- **解绑通知不依赖手机说话**：客户端只等 1 秒，靠连接线程轮询的旧实现在这里必然超时；
- **反复解绑每次都通知**（解绑 → 设为常用设备 → 再解绑 ×3），旧的"只发一次"实现会静默；
- **心跳回报权威身份**：常用设备回报 `primary`、降级后回报 `transient`；
- **发现门控**：有效临时续连凭证得到应答，错误 proof / 错误 client_id / 已作废凭证一律静默；
- 扫码临时设备拿到凭证并可续连、关闭续连时不签发、伪造凭证被拒且不占席位、无连接时"设为常用"被拒。

**Flutter 测试（47 个）**：`app/test/widget_test.dart` 覆盖 `TcpMessage` 编解码（JSON 行、字段往返、畸形行返回 null）；`app/test/lan_check_test.dart` 覆盖「是否不在同一局域网」的判断（移动数据 / 换了 Wi-Fi / 同网 / 热点 / 多网卡 / 无 IPv4 / 地址非法 / 同网段撞车 / **枚举失败时不判断**）、**本机是否有局域网**（只有运营商接口 → 不重试不广播；Wi-Fi / 热点 ap0 / 认不出名字的网卡 → 有局域网）与**广播目标**（热点网段要覆盖、移动数据网段不算）、**重试节奏**（一轮约 30 秒即停、先快后慢）、**判活超时**（前台静默超窗口 → 判死；窗口内 / 后台 / 无基准 → 不判）、**凭证被拒的来源核对**（只有记忆里那台电脑拒的才算被解绑）、**对端关闭的归属**（自己静默被收尸 / 自己刚换网 → 不算电脑侧）、胶囊的词与按钮规则、心跳信息解码。

**尚未自动化**（当前最薄弱的两块）：`home_page` 的发送时机编排（录音状态轮询 → `shouldFlush` 判定，纯函数那半已有 9 条单测、编排那半仍靠真机）、`text_sender` 的「直连 + 发现并行 / 降级 / 续连兜底」编排路径（同样靠真机验证）。

**真机联调**：双端部署、装机、抓日志的固定流程见工作区内的 `.trae/skills/beeptype-deploy`（本地私有，未纳入版本库）；其中的环境事实（无线 adb 地址、Flutter 路径、日志文件位置）只对本机有效。

---

## 9. 与 v0.1 设计稿的差异对照

| v0.1 设计稿 | 现状 |
|---|---|
| 6 位数字配对码 + 手动输码 | 64 位 hex 一次性随机串，仅二维码下发；无手输页面 |
| `paired_device` 单条配对记录 | 常用设备记录 `primary`（token_hash + client_id + hardware_id）+ 手机端多设备列表 |
| 无 token、无免配重连 | token 持久化 + 免码重连；APP 重装靠硬件指纹恢复常用设备身份 |
| 无设备发现 | 凭证门控 UDP 发现（换 IP 无感），对非常用设备设备静默 |
| 无席位概念 | 单席位 + 仲裁 + 接管 + 踢人（`session_ended`）+ 解绑降级（`session_downgraded`） |
| v0.1 的 `clipboard.rs`（非文本保护）、`ack.clipboard_non_text` | 非文本保护未实现（ack 字段为 `code`/`pasted`/`srv_recv_ms`）；现有的 `clipboard.rs` 做的是"写入 + 读回校验"，见 2.5、第 10 节 |
| `tray.rs`（显示配对码/开机自启/关于菜单） | 托盘精简为 5 项，管理操作全部移入主窗口设置页 |
| `qr.html` 独立窗口 | 单页 `index.html`，QR 以 SVG 内联渲染 |
| `build-pc.yml` + `build-mobile.yml` | 合并为单个 `release.yml`，tag 触发 |
| 监听局域网网卡 IP | 监听 `0.0.0.0`（见 5.4） |

---

## 10. 粘贴时序与残余竞态

"写剪贴板 → 注入 Ctrl+V → 注入回车"这条链上有一类**跨机器表现不同**的时序问题。这里记录现状、已经规避掉的部分，以及仍然存在的边界——目的是让后来者不必靠猜来调这两个延时。

### 10.1 所有权传播竞态（已被架构规避）

X11 的剪贴板是"所有者 + 按需服务"模型：粘贴时由接收方向 X server 请求，server 把请求转发给当前所有者，所有者现取现给。所以"写了剪贴板"和"接收方读到新内容"是两件事。社区里常见的坑是 `xclip` 之后立刻 `xdotool key ctrl+v` 会粘出旧内容——因为 `xclip` 默认 fork 到后台持有所有权，命令返回时所有权尚未生效，通用解法是中间 `sleep 0.5`。

我们的实现不落在这一类：

- `set_text` 在返回前已完成 `set_selection_owner` + `flush`（arboard 3.6.1 `x11.rs` 的 `write()`），所有权声明不依赖后续调度；
- `arboard::Clipboard` 实例被 Tauri 插件缓存在插件状态里、随进程存活，其服务线程用 `wait_for_event()` 阻塞等事件（**没有轮询间隔**），收到 SelectionRequest 立即应答；
- 我们是"粘贴"而不是逐字注入，所以也不存在 `xdotool type` 的丢字问题。

`PRE_PASTE_DELAY = 50ms` 因此只是给"目标应用剪贴板缓存 + 归属权切换"的额外余量，不承担正确性。

### 10.2 消费端异步插入竞态（真实存在，用"给足余量 + 焦点复核"处理）

接收方可能把"读剪贴板 → 插入文本"做成异步：Chromium/Electron 系由浏览器进程读剪贴板、再经 IPC 交给渲染进程插入。此时"粘贴后立刻回车"存在回车先于文本落地的窗口 → 表现为**空发送或半截发送**。而我们的自动发送场景（微信桌面版、网页版聊天、网页输入框、VS Code）几乎全是 Chromium/Electron。

这条**无法精确判定**：`arboard` 不暴露"数据被读走"事件（它的 `SetExtLinux::wait()` 等的是"剪贴板被别人覆盖"的 SelectionClear，不是被读走），Windows/macOS 也没有"剪贴板被读"的通知。因此策略是：

- 回车前的等待取"人感知不到的最宽松值"：基准 **250ms** + 每 200 字符 **1ms**（上限 +500ms）。它是安全余量而不是性能参数——快了没有任何收益，慢了就丢消息；
- 注入回车前**复核焦点窗口未变**（`focused_window_id()` 前后各取一次），变了就不发回车。这条检查是"敢给足余量"的前提：余量越长，回车落到用户刚切过去的窗口里的概率越高；
- 粘贴与回车复用**同一条 enigo 连接**，顺序由 X server 在同一连接内保证，不依赖墙钟时间。

**回车被跳过时不上报手机端**：文字就躺在目标窗口的输入框里，用户回到电脑即可看见；手机端没有任何可执行的补救动作（按不了回车），弹提示只会变成噪音。这条判断是在"是否让用户感知每次发送"的讨论里定下的——只有在用户能当场做点什么时才提示。

### 10.3 已知边界

- **校验能覆盖的范围**：`clipboard.rs` 只验证"我们写入的后端里是正确的文字"。Wayland 下若 arboard 退回 XWayland，目标应用读原生 Wayland 剪贴板时的不一致看不出来——那类环境本就不支持自动粘贴，回退手动 Ctrl+V。
- **长文本**：接近 256KB 时走 X11 的 INCR 分块传输，插入更慢；回车余量按长度放宽，但仍是有界的（最坏 750ms）。
- **目标应用自身行为**：有的应用会缓存剪贴板内容、或对粘贴做二次处理，这部分不可观测，也无法适配。
- **用户手上的修饰键**：用户此刻若正按着 Shift，我们注入的 Ctrl+V 会变成 Ctrl+Shift+V。这是"用户手速 vs 注入"的竞态，不是剪贴板竞态。
- **不做按应用/按机器特调**：不维护"检测到 Chromium 系就多等一会儿"这类名单（名单不会完整且会随版本腐坏），也不做首启动自标定或按机型写常数——那只是把特调自动化了。
- **远端桌面/虚拟机**：X2Go / VNC / RDP 的剪贴板是代理转发，延迟不可预测；此时唯一可靠的是"回退手动 Ctrl+V + 长文本分段发送"，不做额外机制。

### 10.4 「不留痕」的两条路都试过了（2026-09-23，**均不采用**，别再重走）

目标曾是"让用户说的字不进目标电脑的剪贴板历史"。两条路都真机实测过，结论如下：

1. **X11 上自己当剪贴板主人**（写入时 `set_selection_owner`，在发出粘贴键之前拒绝一切 `SelectionRequest`）——**机制本身有效**（真机日志：一接管，所有来问的客户端都被拒 ✓），**但对"轮询型"剪贴板管理器无效** ✗：实测 DDE 的 `dde-clipboard` 是**内容变化后 50ms 才开始、每 50ms 轮询一次**，而我们的"放行"为了粘贴必须在写入后约 1ms 就打开 ⇒ 它的请求**全部落在放行之后** ⇒ 照样拿到。
   ⇒ "粘贴前拒绝"只挡得住"写入那一刻立刻来抓"的管理器；要挡轮询型就得连粘贴之后也不放行，那会让**用户手动 Ctrl+V 彻底失效**（内容只在我们内存里），与本项目"剪贴板是兜底"的原则冲突。代码已回滚（`c9ff10d` 撤回）。
2. **键盘注入**（完全不碰剪贴板）——真机实测（`examples/inject_probe.rs`）**中文会丢字、而且不稳定**（随机丢不同的字，重复跑每次不一样）✗ ⇒ 准确度不可接受，**暂停探索**。这也是注入路线的根本弱点：**没有剪贴板那样的读回校验，丢了也不知道**（外加换行可能被当成回车、长文慢、部分应用不吃）。

**结论**：剪贴板 + 粘贴仍是唯一**可靠且稳定**的投递方式；"不留痕"若要继续，只能等更底层的手段（Wayland 门户 / 系统级 IME 通道）或接受折衷。此处保留记录，避免重复投入。
