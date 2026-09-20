# BeeType 技术设计文档

> **版本**：v0.2（与当前实现对齐）
> **日期**：2026-09-20
> **关联文档**：[PRD.md](./PRD.md)

本文描述**代码里实际存在的东西**：模块划分、数据结构、协议字段、状态机与常量取值，全部可在源码中对照。
v0.1 设计稿与最终实现的差异汇总在[第 9 节](#9-与-v01-设计稿的差异对照)。

---

## 1. 总体架构

```
┌──────────────────────────────┐                    ┌──────────────────────────────┐
│      手机端 BeeType App      │                    │     PC 端 BeeType Desktop    │
│         (Flutter)            │                    │        (Tauri + Rust)        │
│                              │                    │                              │
│  pages/  home_page           │   TCP  JSON 行协议  │  server.rs  TCP 服务 + 席位   │
│          scan_page           │ ←────────────────→ │  protocol.rs 消息编解码       │
│  services/ tcp_service       │   hello / text      │  paste.rs   按键模拟(enigo)   │
│            text_sender       │   ack / ping/pong   │  discovery.rs UDP 发现        │
│            storage_service   │   takeover / bye    │  config.rs  身份与配置        │
│            discovery_service │   session_ended     │  lib.rs     托盘/窗口/commands│
│                              │                    │  src/index.html 主窗口前端     │
│  平台通道 beetype/device     │   UDP 发现请求/应答  │                              │
│  (ANDROID_ID / Keychain)     │ ←────────────────→ │  同上端口（默认 8765）         │
└──────────────────────────────┘                    └──────────────────────────────┘
```

两端各自持有一份长期身份与凭证：

| | 手机端 | PC 端 |
|---|---|---|
| 本机身份 | `client_id`（首启生成的 UUIDv4）+ `hardware_id`（Android ANDROID_ID / iOS Keychain UUID） | `device_id`（首启生成的 UUIDv4）+ `device_name`（hostname） |
| 对端凭证 | 已绑定电脑列表（含明文 token） | 唯一主力手机记录（含 `token_hash`） |
| 临时关系 | 不落盘，仅内存 | 不落盘，仅内存 |

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
        └── discovery.rs    # UDP 凭证门控发现
```

> v0.1 设计中的 `tray.rs`、`qr.rs` 不存在：托盘与 QR 生成都在 `lib.rs`（QR 直接用 `qrcode` crate 渲染成 SVG 内联进 WebView）。`clipboard.rs` 现在是存在的，但它做的是"写入 + 读回校验"，**不是** v0.1 设想的"非文本保护"。

### 2.2 config.rs — 身份、凭证与配置

配置目录（`BEETYPE_HOME` 环境变量可覆盖，单测用）：

| 平台 | 路径 |
|---|---|
| Linux | `~/.config/beetype/config.json` |
| macOS | `~/Library/Application Support/beetype/config.json` |
| Windows | `%HOME%/AppData/Roaming/beetype/config.json` |

```rust
pub struct Config {
    pub port: u16,                    // 默认 8765（TCP 与 UDP 同号）
    pub autostart: bool,              // 默认 false
    #[serde(default = "default_true")]
    pub auto_paste: bool,             // 默认 true（实际生效还需平台支持）
    #[serde(default)]
    pub auto_send: bool,              // 默认 false，依赖 auto_paste
    pub code_hash: Option<String>,    // SHA-256(配对码)
    pub code_expires_at: Option<u64>, // 毫秒时间戳
    pub device_id: String,            // 本机 UUIDv4
    pub device_name: String,          // 默认 hostname
    pub primary: Option<PrimaryDevice>, // 唯一主力手机（None = 主力位空缺）
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
| `unbind_primary()` | 清空主力记录，旧凭证立即失效 |

随机与哈希都是手写实现（`/dev/urandom` + `sha2`），没有引入 uuid/rand 依赖；旧版配置里的未知字段（如 `paired_device`）由 serde 默认忽略，向后兼容。

### 2.3 protocol.rs — 消息编解码

- 传输：每条消息一行 JSON + `\n`，UTF-8，单行上限 `MAX_LINE_BYTES = 256 * 1024`。
- 使用 serde 内部标记 `#[serde(tag = "type")]`；新字段全部 `#[serde(default)]`，旧客户端缺字段也能解析（有单测覆盖）。

消息类型（完整字段见[第 3 节](#3-通信协议)）：

| 变体 | 方向 |
|---|---|
| `hello` / `hello_ack` | 手机 → PC / PC → 手机 |
| `text` / `ack` | 手机 → PC / PC → 手机 |
| `ping` / `pong` | 手机 → PC / PC → 手机 |
| `takeover` / `takeover_ack` | 手机 → PC / PC → 手机 |
| `bye` / `bye_ack` | 手机 → PC / PC → 手机 |
| `session_ended` | PC → 手机（被踢前通知） |
| `session_downgraded` | PC → 手机（解绑降级，会话保持） |

### 2.4 server.rs — TCP 服务、认证与席位

#### 常量

| 常量 | 值 | 含义 |
|---|---|---|
| `MAX_AUTH_FAILS` | 5 | 同 IP 累计鉴权失败次数 |
| `COOLDOWN_SECS` | 60 | 触发失败上限后的冷却时长 |
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

`KICK_TAKEOVER = 1`（主力接管）、`KICK_DISCONNECT = 2`（托盘/窗口断开）、`KICK_REPLACED = 4`（同主力僵尸连接被替换），映射为 `session_ended.reason` 的 `owner_takeover` / `owner_disconnect` / `replaced`。

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

#### 认证：两条分支

`handle_hello` 先查冷却（冷却中直接回 `cooldown`），再按凭证分流：

**token 分支（主力免配重连）**

1. `verify_token(client_id, token, ...)` 失败 → 记一次失败，回 `unauthorized`。
2. 成功则清空该 IP 的失败计数，按下述仲裁结果处理：席位空闲 → 直接接入（`session = primary`）；席位被**同一 `client_id`** 占着（僵尸重连）→ 踢旧接新；席位被他人（临时方）占用 → 回 `status = "occupied"`，**连接保持**并标记 `waiting_takeover`，等手机发 `takeover`。

**code 分支（扫码接入）**

1. `verify_code(code)` 失败 → 记失败，回 `bad_code`。
2. 席位已被占用 → 回 `occupied`，不认证、不绑定、**不换码**。
3. 席位空闲时判定身份：
   - `returning`：来者的 `client_id` 命中主力记录，**或** `hardware_id` 命中主力记录（APP 重装后 `client_id` 会变，硬件指纹不会）→ 直接恢复主力席位，**不下发 token**（手机本来就有）。
   - `bind`：勾选了「记住这台手机」且主力位空缺且有 `client_id` → 绑定为新主力并一次性下发 token。
   - 其余 → 临时会话（`session = "transient"`），不落盘任何东西。
4. 无论哪种结果，配对码都立即 `regenerate_code()`（**一次性**，杜绝重放），并写回内存中的二维码内容。

两分支成功后都会：写席位、刷新托盘、通知前端（`pairing-show`）、日志记录。

#### `takeover`（主力接管被占用的电脑）

仅当连接状态为「已认证 + 是主力 + 正在等待接管」时才受理：踢掉当前持有者（`owner_takeover`）、把席位交给发起方、回 `takeover_ack{ok:true}`；否则回 `ok:false`。**临时方发起的 takeover 必然被拒**——协议层不存在手机端篡位的路径。

#### 收到 `text` 的处理链

1. 非席位持有者 / 未认证 / 等待接管中的连接 → 回 `ack{ok:false, code:"not_connected"}`。
2. 文本 `trim()` 为空 → 回 `ack{ok:false, code:"empty_text"}`，不做任何剪贴板操作。
3. 写入剪贴板并**读回校验**（`clipboard.rs`）：读到别的内容（被其他进程抢先持有）就重写一次；写入报错或重写后仍不一致 → 弹系统通知，回 `ack{ok:false, code:"clipboard_error"}`。读不回来则按原样继续（fail-open，不因"校验不了"把能用的粘贴挡掉）。
4. 自动粘贴条件全部满足时才模拟粘贴：设置页开关生效 + 当前会话支持按键注入 + **BeeType 自己的窗口没有焦点**（避免把文字糊进配对窗口）。注入前记下焦点窗口，成功则 `pasted = true`，并交回 enigo 实例。
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
2. `primary` 为空（还没绑主力）→ 静默丢弃。
3. `(client_id, proof)` 与主力记录不匹配 → 静默丢弃（**电脑对陌生设备隐身，不产生任何应答**）。
4. 完全匹配 → 仅对请求来源**单播**：

```json
{"type":"vl_disc_resp","protocol":1,"device_id":"<pc uuid>","device_name":"hejiong-PC",
 "device_type":"pc","port":8765,"busy":false,"capabilities":{"auto_paste":true}}
```

`busy` 只给布尔值，不泄露占用者身份。用 `SHA-256(token)` 而非 token 本身作 proof：抓到广播不足以通过 TCP 认证。发现线程与 TCP 服务同端口，绑定失败只记日志，不影响 TCP 主服务。

### 2.8 lib.rs — 装配、托盘与 commands

启动流程：初始化日志 → 加载配置 → `regenerate_code()`（**每次启动都换新码**）→ 按配置同步系统自启状态 → 建 `ServerState` → 建托盘 → spawn TCP 服务线程与 UDP 发现线程。

托盘菜单（GTK 下状态行必须保持 enabled，否则整项连 emoji 一起被渲染成灰色，点击由兜底分支吞掉）：

```
⚪ 等待配对 | 🟢 <手机名> | 🟢 <手机名> · 临时
打开主窗口
断开当前手机            ← 无会话时禁用
──────
退出
```

托盘图标左键单击也会唤起主窗口；窗口关闭按钮只是隐藏（`prevent_close` + `hide`），真正退出走托盘「退出」。

主窗口（380×520，可缩放，最小 340×120，启动即可见）：

- 前端每 1s 轮询 `get_pairing_info`；轮询顺带调用 `ensure_code_fresh()`——只要没连接、且配对码已过期或剩余不足 2 分钟就自动换新，保证窗口上展示的二维码永远可用（PC 长期运行时不会变成死码）。
- 配对成功后先通知前端渲染绿色「已连接」页，0.9s 后自动隐藏到托盘；除非窗口是用户从托盘手动打开的（`manual_window` 标记，点叉关闭时清除）。
- QR 内容格式 `beetype://<ip>:<port>?code=<code>`，由 `qrcode` 渲染成 SVG 内联返回，无需前端构建链。
- TCP 绑定 `0.0.0.0:<port>`；`lan_ip()` 仅是取默认路由网卡的 IPv4（`UdpSocket::connect` 只做路由选择，**不发包**）用于生成二维码里的地址。

Tauri commands（10 个）：

| command | 作用 |
|---|---|
| `get_status` | 粗粒度状态文本（等待配对/已连接） |
| `get_pairing_info` | 二维码内容 + SVG + 主力位 + 连接摘要（顺带续期配对码） |
| `get_settings` | 自动粘贴/自动发送的实际生效值、平台支持情况、开机自启、主力机名、版本号 |
| `set_auto_paste` | 存用户意愿；运行态存「意愿 && 平台支持」；关闭时联动关闭 auto_send |
| `set_auto_send` | 存用户意愿；运行态存「意愿 && auto_paste 生效 && 平台支持」 |
| `set_autostart` | 操作系统自启项 + 持久化 |
| `set_remember_next` | 「记住这台手机」勾选（仅对下一次码配对生效） |
| `disconnect_session` | 托盘/窗口断开当前手机 |
| `unbind_primary` | 解绑主力（在线会话降级为临时，不断开） |
| `set_window_height` | 前端启动时量一次真实高度后固定窗口高度 |

### 2.9 前端（desktop/src/index.html）

- 主页与设置页**绝对定位叠放**，切换只改 `opacity`/`visibility`（交叉淡化），元素几何全程不变——规避 WebKitGTK 层重建导致的标题闪烁。
- 窗口高度在启动时用离屏克隆渲染「等待态」量出真实高度（两种勾选文案取高者）后固定，之后不再随状态跳变：主页内容裁切、设置页内部滚动。
- 渲染做了结构键缓存（`lastStatusKey`/`lastPairKey`），避免 1s 轮询全量重绘导致 hover/勾选态丢失；只有「无连接 → 有连接」这类跨状态切换才播入场动画。
- 设置页每次进入都拉一次 `get_settings`；「记住这台手机」勾选框仅在主力位空缺时出现，默认不勾（扫码默认即临时连接）。

---

## 3. 通信协议

### 3.1 传输与编码

TCP 长连接，UTF-8，每条消息一行 JSON 以 `\n` 结尾，单行上限 256KB。PC 端对超长行**直接断开连接**（不截断）。

### 3.2 消息字段

#### hello（手机 → PC）

```json
{"type":"hello","code":"<64 hex>","client_id":"<uuid>","device_name":"Xiaomi 14","hardware_id":"<android_id>"}
{"type":"hello","token":"<64 hex>","client_id":"<uuid>","device_name":"Xiaomi 14","hardware_id":"<android_id>"}
```

`code` 与 `token` 二选一；`client_id`/`device_name`/`hardware_id` 均可缺省（旧客户端兼容）。

#### hello_ack（PC → 手机）

| 字段 | 类型 | 说明 |
|---|---|---|
| `ok` | bool | 是否成功 |
| `status` | string | `ok` / `occupied` / `bad_code` / `unauthorized` / `cooldown` |
| `error` | string? | 人话错误描述（如「尝试过于频繁，请 60 秒后再试」） |
| `session` | string? | `primary` / `transient`（仅成功时出现） |
| `device_id` / `device_name` / `device_type` | string? | PC 身份，`device_type` 固定 `pc` |
| `token` | string? | **仅新绑定主力那一次**出现，明文只下发一次 |
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
| `{"type":"ping"}` / `{"type":"pong"}` | 手机 ↔ PC | 心跳，任何连接都可发 |
| `{"type":"takeover"}` / `{"type":"takeover_ack","ok":bool}` | 手机 → PC → 手机 | 主力接管被占用的席位 |
| `{"type":"bye"}` / `{"type":"bye_ack"}` | 手机 → PC → 手机 | 优雅断开，PC 随即释放席位 |
| `{"type":"session_ended","reason":"..."}` | PC → 手机 | 被踢前通知；`owner_takeover` / `owner_disconnect` / `replaced` |
| `{"type":"session_downgraded"}` | PC → 手机 | 主力被解绑，当前会话降级为临时（连接保持） |

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
│   └── scan_page.dart             # 扫码页（mobile_scanner）
└── services/
    ├── tcp_service.dart           # TCP、分帧、请求/应答 Completer、心跳
    ├── text_sender.dart           # 编排：自动连接、配对、降级、发送
    ├── storage_service.dart       # client_id、已绑定电脑列表、设备名
    └── discovery_service.dart     # UDP 广播发现
```

平台通道 `beetype/device`：Android 在 `MainActivity.kt` 实现 `getDeviceName`（按品牌优先级取系统设备名）与 `getHardwareId`（ANDROID_ID）；iOS 在 `AppDelegate.swift` 实现（`UIDevice.current.name` 与 Keychain 持久化 UUID）。

### 4.2 依赖

| package | 用途 |
|---|---|
| mobile_scanner | 二维码扫描 |
| shared_preferences | 身份与已绑定电脑持久化 |
| network_info_plus | 取 WiFi IP 以推算定向广播地址 |
| crypto | 计算 `SHA-256(token)` 作为发现 proof |
| `dart:io` Socket / RawDatagramSocket | TCP 与 UDP（标准库） |

`connectivity_plus` 仍在依赖里，但连接状态判定已不依赖它（靠 socket 事件与心跳）。

### 4.3 tcp_service.dart

- 分帧：把收到的字节拼进 `StringBuffer`，按 `\n` 切行，余下部分留作下一批。
- 请求/应答配对：`hello`、`takeover`、`bye`、`text` 各自用 Completer 等待对应的 `hello_ack` / `takeover_ack` / `bye_ack` / `ack`（按 `id` 索引）。
- 超时：`hello` 默认 10s、`takeover` 8s、`text` ack 5s、`bye` 等 `bye_ack` 兜底 1.5s；任何超时都会按失败结果返回并清理资源，不抛给 UI。
- 心跳：每 5s 发一次 `ping`（PC 端 15s 收尸，切后台被 Doze 延迟也留有 3 倍余量）。**没有 pong 超时**——断线由 socket 的 `onDone`/`onError`（配合 TCP keepalive）感知，`pong` 只用来证明链路活跃。
- 状态机：`disconnected → connecting → (pairing) → paired`，另有 `occupied`（已认证但席位被占，连接保持）与 `failed`。
- `session_ended` 与 `session_downgraded` 分别以 reason 字符串（含 `downgraded`）推给 UI 层。

### 4.4 text_sender.dart — 编排层

`autoConnect()`（打开 APP 的默认路径）：

1. 取最近使用的一台已绑定电脑（`last_connected_ms` 最大者）。
2. **并行**：直接连缓存 IP（TCP 连接超时 2.5s，`hello` 5s）＋ UDP 发现（约 1.1s 出结果）。
3. 直连结论明确（成功 / occupied / 凭证被拒）就结束，不再换 IP 试。
4. 直连网络层失败才用发现结果补连一次（DHCP 换 IP 的主路径），连上后刷新缓存里的 IP。
5. 凭证被拒（`unauthorized`）→ 由上层删除本地凭证并停止重连。

其他入口：`pairWithCode()`（扫码配对，只有拿到 token 才落盘）、`pairFromQr()`（解析 `beetype://` URI）、`takeover()`、`userDisconnect()`（手动断开并标记本进程不再自动重连）、`downgradeToTemp()`（PC 解绑后删本地凭证、会话转临时，不断开）、`forgetDevice()`。

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

`home_page.dart` 的 UI 骨架不随连接状态跳变，只有顶部状态胶囊变色与标签、主体区换视图：

| 状态 | 状态胶囊 | 主体 |
|---|---|---|
| `paired` | 绿点 + 电脑名（临时会话加「临时连接」灰标）+「断开」 | 可输入的输入卡 |
| `occupied` | 橙点 + 电脑名 +「另一台手机正在使用」 | 「电脑使用中」页 +「接管电脑」按钮 |
| `connecting`/`pairing` | 转圈 +「正在连接…」 | 输入卡（禁用） |
| `disconnected` 且无绑定 | 灰点 +「未连接电脑」 | 扫码引导页 |
| `disconnected` 且有绑定 | 灰点 + 电脑名 +「连不上，重试中…」或「重新连接」 | 输入卡（禁用） |

自动重连：退避 1s → 2s → 4s → 8s，之后固定 10s 一直重试（不做「N 次后放弃」）；切回前台立即补试一次；手动断开后完全停止。

扫码页：同一张二维码只发起一次配对请求（去重），否则取景框不挪开时会连发请求，5 次 `bad_code` 就把自己关进 60 秒冷却。

### 4.8 语音发送时序（关键设计）

```
按住输入法麦克风 → composing 持续有效（换气停顿也不断）
松开 → 输入法提交文本、composing 变 OFF（下降沿）
     → 立即（0ms 延时）发送输入框当前全文
普通键盘输入（无 composing） → 300ms 防抖后发送快照
```

判据是 composing 的**下降沿**而不是静音停顿，因此不需要秒级观察窗，代价是对「句末停顿先提交一句、随后追加新段」类输入法（搜狗等）不适用——适配矩阵见 PRD NFR-19。发送成功后清空输入框；失败保留全文并提示，右侧有兜底发送按钮。

### 4.9 输入框聚焦与软键盘

连接成功后自动聚焦并强制唤起软键盘。冷启动存在竞态（Activity 尚未完成 IME 注册时 `TextInput.show` 会被系统取消），因此用 `viewInsets.bottom` 验证键盘是否真的弹出，未弹出则以 350ms 间隔重试，最多 3 次。切走方向（resumed → inactive）主动收起键盘，消除转场快照导致的键盘闪动。

---

## 5. 安全设计

### 5.1 凭证生命周期

```
启动：每次启动都 regenerate_code() → 新配对码（旧码立即失效）
展示：窗口轮询时若码已过期或剩余 <2 分钟则自动换新（未连接时才换）
配对：hello{code} → 校验通过 → 立即换新码（一次性）
        勾选「记住」且主力位空缺 → 生成 token，明文随 hello_ack 下发一次
        主力位已有主人 → 临时会话，两端都不落盘
重连：hello{token} → SHA-256 比对命中 → 免码接入
解绑：config.primary 清空 → 在线主力会话降级为临时（session_downgraded，不断开）→ 旧 token 立即失效
```

### 5.2 权限边界

- 绑定主力的唯一入口是 **PC 端窗口的勾选框**，协议里没有任何手机端可以修改信任关系的指令。
- 临时手机拿不到 token，也发不出有效 proof，设备发现对它不存在。
- 主力遇到席位被占只能等或接管，**绝不自动踢人**；临时方发 `takeover` 一律被拒。
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
| 监听地址 | 仅绑局域网网卡 | `0.0.0.0`（TCP 与 UDP） | 多网卡/虚拟网卡环境（Docker、VPN）下判定「哪个是局域网网卡」易出错；改由高熵凭证 + 冷却承担 |
| 解绑行为 | 踢掉在线主力 | 降级为临时连接，不断开 | 用户解绑通常是想换机，打断正在进行的输入没有收益 |
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

---

## 7. 构建与发布

- **CI**：`.github/workflows/release.yml`，仅在推 `v*` tag 或手动触发时运行。
  - `version` 从 tag 提取（`v0.2.0` → `0.2.0`），写入 `app/pubspec.yaml` 与 `desktop/src-tauri/tauri.conf.json`（**版本以 tag 为唯一来源**，仓库里的字面量不参与发布）。
  - Android：`flutter build apk --release --target-platform android-arm64`，签名 keystore 由 Secrets 还原并生成 `key.properties`。
  - 桌面：矩阵 `ubuntu-22.04`（GLIBC 兼容性）/ `macos-latest` / `windows-latest`，`tauri build` 后分别产出 `.deb` + `.AppImage`、`.dmg`、`.msi`。
  - Release job 汇总所有产物，`workflow_dispatch` 触发时先建 draft。
- **本地构建**：桌面端 `cd desktop && cargo tauri build`（开发期 `cargo build` 后直接跑 `target/debug/beetype`，前端是单文件 HTML，无需前端构建链）；手机端 `cd app && flutter build apk --release --target-platform android-arm64`。
- 仓库根的行尾与二进制属性由 `.gitattributes` 统一（`* text=auto eol=lf`）；`.trae/`、`target/`、签名文件在 `.gitignore` 中排除。

---

## 8. 测试与验证

**Rust 单测（20 个，`cargo test`）**

| 文件 | 覆盖 |
|---|---|
| `config.rs` | 配对码校验与过期、UUIDv4 形态、主力绑定/校验/解绑全生命周期、硬件指纹随主力记录持久化、落盘不含明文 token、旧配置迁移 |
| `protocol.rs` | 各消息 encode/decode 往返、旧客户端字段缺省兼容、畸形 JSON 不 panic |
| `server.rs` | 席位仲裁三分支（空席接入/同主力替换/被临时方占用）、踢人原因映射、QR 内容格式 |
| `discovery.rs` | 请求解析的合法/畸形样本、应答 JSON 字段 |
| `paste.rs` | 回车余量的边界（基准值、按长度递增、上限、整体 <1s） |

**Flutter 测试**：`app/test/widget_test.dart` 覆盖 `TcpMessage` 编解码（JSON 行、字段往返、畸形行返回 null）。

**尚未自动化**（当前最薄弱的两块）：`home_page` 的 composing 下降沿发送判定、`text_sender` 的「直连 + 发现并行 / 凭证被拒 / 降级」编排路径。

**真机联调**：双端部署、装机、抓日志的固定流程见工作区内的 `.trae/skills/beetype-deploy`（本地私有，未纳入版本库）；其中的环境事实（无线 adb 地址、Flutter 路径、日志文件位置）只对本机有效。

---

## 9. 与 v0.1 设计稿的差异对照

| v0.1 设计稿 | 现状 |
|---|---|
| 6 位数字配对码 + 手动输码 | 64 位 hex 一次性随机串，仅二维码下发；无手输页面 |
| `paired_device` 单条配对记录 | 主力手机记录 `primary`（token_hash + client_id + hardware_id）+ 手机端多设备列表 |
| 无 token、无免配重连 | token 持久化 + 免码重连；APP 重装靠硬件指纹恢复主力身份 |
| 无设备发现 | 凭证门控 UDP 发现（换 IP 无感），对非主力设备静默 |
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
