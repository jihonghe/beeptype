# BeeType 技术设计文档

> **版本**：v0.1
> **日期**：2026-09-18
> **关联文档**：[PRD.md](./PRD.md)

---

## 1. PC 端详细设计（Tauri + Rust）

### 1.1 模块职责划分

```
┌──────────────────────────────────────────────┐
│                 Tauri App                      │
│                                               │
│  ┌──────────┐  ┌──────────┐  ┌────────────┐  │
│  │ config   │  │ qr       │  │ tray       │  │
│  │ 配置管理  │  │ QR 码生成│  │ 托盘+菜单  │  │
│  └──────────┘  └──────────┘  └────────────┘  │
│                                               │
│  ┌──────────┐  ┌──────────┐  ┌────────────┐  │
│  │ protocol │  │ server   │  │ clipboard  │  │
│  │ 协议编解码│  │ TCP 服务 │  │ 剪贴板写入  │  │
│  └──────────┘  └──────────┘  └────────────┘  │
│                                               │
│  ┌──────────────────────────────────────────┐ │
│  │           Tauri 插件层                    │ │
│  │  clipboard-manager / autostart           │ │
│  │  notification / single-instance          │ │
│  └──────────────────────────────────────────┘ │
└──────────────────────────────────────────────┘
```

### 1.2 模块详细设计

#### 1.2.1 config.rs — 配置管理

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub port: u16,                    // 监听端口，默认 8765
    pub paired_device: Option<PairedDevice>,
    pub autostart: bool,              // 是否开机自启
    pub code_hash: Option<String>,    // 当前配对码的 SHA-256 hash
    pub code_expires_at: Option<u64>, // 配对码过期时间戳（毫秒）
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PairedDevice {
    pub name: String,
    pub code_hash: String,
    pub last_ip: String,
    pub paired_at: u64,
}

impl Config {
    /// 加载或创建配置文件
    /// 路径：~/.config/beetype/config.json（Linux）
    ///       ~/Library/Application Support/beetype/config.json（macOS）
    ///       %APPDATA%/beetype/config.json（Windows）
    pub fn load_or_create() -> Result<Self, ConfigError>;

    /// 原子写入配置文件（先写临时文件再 rename）
    /// 文件权限 0600，目录权限 0700
    pub fn save(&self) -> Result<(), ConfigError>;

    /// 生成新配对码（6 位随机数字）
    /// 同时更新 code_hash 和 code_expires_at（5 分钟后过期）
    pub fn regenerate_code(&mut self) -> String;

    /// 校验配对码
    pub fn verify_code(&self, code: &str) -> bool;

    /// 标记配对成功
    pub fn mark_paired(&mut self, device_name: String, ip: String, code: &str);

    /// 清除配对
    pub fn clear_pairing(&mut self);
}
```

**配置文件示例**：
```json
{
  "port": 8765,
  "paired_device": {
    "name": "陈先生的手机",
    "code_hash": "a1b2c3...",
    "last_ip": "192.168.31.50",
    "paired_at": 1730000000000
  },
  "autostart": false,
  "code_hash": "d4e5f6...",
  "code_expires_at": 1730000300000
}
```

#### 1.2.2 protocol.rs — 通信协议

```rust
use serde::{Deserialize, Serialize};

pub const MAX_LINE_BYTES: usize = 256 * 1024; // 256KB

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Message {
    hello { code: String, device: String },
    hello_ack { ok: bool, device: String },
    text { id: String, text: String, ts: u64 },
    ack { id: String, ok: bool, clipboard_non_text: bool, error: Option<String> },
    ping,
    pong,
}

/// 将消息编码为一行 JSON + \n
pub fn encode_line(msg: &Message) -> Result<String, ProtocolError>;

/// 从一行字符串解码消息
pub fn decode_line(line: &str) -> Result<Message, ProtocolError>;

/// 当前毫秒时间戳
pub fn now_ms() -> u64;
```

**设计要点**：
- 使用 `serde` 的 `tag = "type"` 内部标记，JSON 中 `type` 字段决定变体
- `encode_line` 保证输出不含 `\n`（serde_json 默认转义）
- `decode_line` 遇到非法 JSON 或未知 type 返回 `ProtocolError`，不 panic

#### 1.2.3 server.rs — TCP 服务

```rust
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const HELLO_TIMEOUT_S: u64 = 10;   // 连接后 10 秒内必须 hello
const IO_TIMEOUT_S: u64 = 90;      // 单次读写超时
const MAX_FAIL_PER_IP: u32 = 5;    // 同 IP 最大失败次数
const COOLDOWN_S: u64 = 60;         // 冷却时间
const KEEPALIVE_IDLE: u64 = 10;     // TCP keepalive 空闲秒
const KEEPALIVE_INTERVAL: u64 = 5;  // 探测间隔
const KEEPALIVE_COUNT: u32 = 3;     // 探测次数

struct FailTracker {
    fails: HashMap<String, (u32, Instant)>, // IP → (次数, 首次失败时间)
}

pub struct BridgeServer {
    config: Arc<Mutex<Config>>,
    fail_tracker: Mutex<FailTracker>,
    connected_device: Mutex<Option<String>>, // 当前已连接的设备名
}

impl BridgeServer {
    /// 在局域网 IP 上监听
    /// 获取本机局域网 IP（遍历网卡，找非 loopback 的 IPv4）
    pub fn lan_ip() -> Option<String>;

    /// 启动服务
    pub fn serve(&self, port: u16) -> Result<(), ServerError>;

    /// 处理单个连接
    fn handle_connection(&self, stream: TcpStream);

    /// 校验配对码 + 防暴力
    fn check_hello(&self, code: &str, ip: &str) -> HelloResult;

    /// 处理 text 消息
    fn handle_text(&self, id: &str, text: &str) -> AckMessage;

    /// 处理 ping
    fn handle_ping(&self) -> Message;
}

enum HelloResult {
    Ok,
    WrongCode,
    Cooldown(u64),   // 还需冷却多少秒
    Expired,         // 配对码已过期
    AlreadyPaired,   // 已有设备连接
}
```

**连接处理流程**：
```
accept 连接
  → 设置 TCP keepalive
  → 设置读写超时 10s（等待 hello）
  → 读第一行 → decode
  → if Message::hello:
      → check_hello(code, ip)
      → Ok: 存 connected_device, 回 hello_ack, 读写超时改为 90s
      → WrongCode: 记录失败, 回 hello_ack{ok:false}, 断连
      → Cooldown: 回 hello_ack{ok:false, error:"cooldown"}, 断连
      → Expired: 回 hello_ack{ok:false, error:"expired"}, 断连
  → else: 断连（第一消息必须是 hello）

  循环读消息:
    → text: handle_text → 回 ack
    → ping: 回 pong
    → 其他/畸形: 忽略，继续读（不杀连接）
    → EOF/超时: 断连，清 connected_device
```

**日志规范**（脱敏）：
```rust
log::info!("recv id={} type={} len={}", id, msg_type, text.len());
// 永远不 log text 内容
```

#### 1.2.4 clipboard.rs — 剪贴板

```rust
use tauri_plugin_clipboard_manager::ClipboardExt;

pub struct ClipboardWriter {
    app: tauri::AppHandle,
}

impl ClipboardWriter {
    /// 写入文本到系统剪贴板
    /// 返回 AckResult
    pub fn write_text(&self, text: &str) -> AckResult {
        // 1. 尝试检测当前剪贴板是否有非文本内容
        //    Tauri clipboard-manager 不直接支持 target 检测
        //    Linux: 调用 xclip -selection clipboard -o -t TARGETS
        //    macOS: 不需要（pbcopy 直接覆盖）
        //    Windows: 不需要（WriteClipboard 直接覆盖）
        //
        // 2. 如果检测到非文本 target 且不是纯文本：
        //    → 不覆盖，返回 AckResult { ok:true, clipboard_non_text:true }
        //
        // 3. 写入文本：
        //    → app.clipboard().write_text(text)
        //    → 返回 AckResult { ok:true, clipboard_non_text:false }
        //
        // 4. 写入异常：
        //    → 返回 AckResult { ok:false, error:"clipboard_write_failed" }
    }
}

pub struct AckResult {
    pub ok: bool,
    pub clipboard_non_text: bool,
    pub error: Option<String>,
}
```

**平台差异处理**：
```
Linux:
  - 检测非文本: xclip -selection clipboard -o -t TARGETS
  - 写入: Tauri clipboard-manager（内部调用 xclip/xsel）
  - 无 xclip 时: 降级为仅 xsel，或提示用户安装 xclip

macOS:
  - 检测非文本: 不需要（pbcopy 直接覆盖任何格式）
  - 写入: Tauri clipboard-manager（内部调用 pbcopy）

Windows:
  - 检测非文本: 不需要（直接覆盖）
  - 写入: Tauri clipboard-manager（内部调用 Win32 API）
```

#### 1.2.5 tray.rs — 系统托盘

```rust
use tauri::{
    tray::{TrayIconBuilder, MouseButton, TrayIconEvent},
    menu::{Menu, MenuItem, PredefinedMenuItem, CheckMenuItem},
    Manager,
};

pub enum TrayState {
    WaitingForPair,     // 灰色
    PairedWaiting,      // 黄色
    Connected,          // 绿色
    TextReceived,       // 蓝色闪
    ClipboardBlocked,   // 橙色闪
}

pub fn build_tray(app: &tauri::AppHandle) -> Result<(), tauri::Error> {
    let menu = Menu::with_items(app, &[
        &MenuItem::new(app, "显示配对码", true)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::new(app, "状态：等待配对", false)?,  // disabled
        &MenuItem::new(app, "设备：—", false)?,          // disabled
        &PredefinedMenuItem::separator(app)?,
        &CheckMenuItem::new(app, "开机自启", true, false)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::new(app, "关于", true)?,
        &MenuItem::new(app, "退出", true)?,
    ])?;

    TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .tooltip("BeeType - 等待配对")
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "显示配对码" => { show_qr_window(app); }
                "开机自启" => { toggle_autostart(app); }
                "退出" => { app.exit(0); }
                _ => {}
            }
        })
        .build(app)?;
    Ok(())
}

pub fn update_tray_state(app: &tauri::AppHandle, state: TrayState) {
    // 更新图标 + tooltip
    // 更新菜单项文本（状态/设备名）
}
```

#### 1.2.6 qr.rs — QR 码生成

```rust
use qrcode::QrCode;
use image::Luma;

/// 生成 QR 码图片（PNG bytes）
pub fn generate_qr(data: &str) -> Vec<u8> {
    let code = QrCode::new(data).unwrap();
    let image = code.render::<Luma<u8>>().build();
    // 转 PNG bytes
    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .encode(&image, image.width(), image.height(), image::ExtendedColorType::L8)
        .unwrap();
    png_bytes
}

/// 生成配对 JSON 字符串
pub fn build_pair_payload(ip: &str, port: u16, code: &str) -> String {
    serde_json::json!({
        "ip": ip,
        "port": port,
        "code": code
    }).to_string()
}
```

### 1.3 Tauri 配置

#### tauri.conf.json

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "BeeType",
  "version": "0.1.0",
  "identifier": "com.beetype.app",
  "build": {
    "frontendDist": "src"
  },
  "app": {
    "withGlobalTauri": true,
    "windows": [],
    "trayIcon": {
      "iconPath": "icons/icon.png",
      "iconAsTemplate": true
    },
    "security": {
      "csp": null
    }
  },
  "plugins": {
    "clipboard-manager": {},
    "autostart": { "macosExtraArgs": ["-hide"] }
  }
}
```

**关键配置说明**：
- `windows: []`：启动时不显示主窗口，仅托盘
- `frontendDist: "src"`：前端文件在 src 目录（QR 码窗口的 HTML）
- `trayIcon`：声明托盘图标

#### Cargo.toml

```toml
[package]
name = "beetype"
version = "0.1.0"
edition = "2021"

[dependencies]
tauri = { version = "2", features = ["tray-icon"] }
tauri-plugin-clipboard-manager = "2"
tauri-plugin-autostart = "2"
tauri-plugin-notification = "2"
tauri-plugin-single-instance = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
qrcode = "0.14"
image = "0.25"
sha2 = "0.10"
log = "0.4"
env_logger = "0.11"

[build-dependencies]
tauri-build = "2"
```

### 1.4 前端设计（QR 码窗口）

QR 码窗口是一个极简 HTML 页面，由 Tauri WebView 渲染：

```html
<!-- src/qr.html -->
<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    body { margin: 0; display: flex; justify-content: center; align-items: center;
           min-height: 100vh; background: #1a1a2e; color: #eee;
           font-family: system-ui, sans-serif; }
    .container { text-align: center; }
    #qr { width: 240px; height: 240px; image-rendering: pixelated; }
    .info { margin-top: 16px; font-size: 14px; color: #888; }
    .code { font-size: 28px; font-weight: bold; letter-spacing: 8px; color: #0f0; }
  </style>
</head>
<body>
  <div class="container">
    <img id="qr" alt="QR Code" />
    <div class="code" id="code"></div>
    <div class="info">用手机 BeeType 扫描此二维码</div>
  </div>
  <script>
    // 通过 Tauri invoke 获取 QR 码数据和图片
    window.__TAURI__.invoke('get_qr_data').then(({png_base64, code, ip, port}) => {
      document.getElementById('qr').src = 'data:image/png;base64,' + png_base64;
      document.getElementById('code').textContent = code;
    });
  </script>
</body>
</html>
```

---

## 2. 手机端详细设计（Flutter）

### 2.1 依赖清单

```yaml
# pubspec.yaml
dependencies:
  flutter:
    sdk: flutter
  mobile_scanner: ^6.0.0        # QR 码扫描
  shared_preferences: ^2.3.0    # 配对凭证持久化
  connectivity_plus: ^6.0.0     # 网络状态监听
  uuid: ^4.0.0                  # 消息 ID 生成

dev_dependencies:
  flutter_test:
    sdk: flutter
```

> `dart:io` 的 `Socket` 是标准库，不需要额外依赖。

### 2.2 核心类设计

#### 2.2.1 TcpService — TCP 连接管理

```dart
import 'dart:io';
import 'dart:async';
import 'dart:convert';

enum ConnectionState {
  disconnected,
  connecting,
  connected,
  reconnecting,
  failed,
}

class TcpService {
  Socket? _socket;
  Timer? _heartbeatTimer;
  Timer? _pongTimer;
  int _retryCount = 0;
  String? _ip;
  int? _port;
  String? _code;

  final _stateController = StreamController<ConnectionState>.broadcast();
  final _messageController = StreamController<Map<String, dynamic>>.broadcast();

  Stream<ConnectionState> get stateStream => _stateController.stream;
  Stream<Map<String, dynamic>> get messageStream => _messageController.stream;

  /// 连接到 PC
  Future<bool> connect(String ip, int port, String code, {String? deviceName}) async {
    _ip = ip; _port = port; _code = code;
    _stateController.add(ConnectionState.connecting);
    try {
      _socket = await Socket.connect(ip, port, timeout: Duration(seconds: 3));
      _sendHello(deviceName);
      _startListening();
      _startHeartbeat();
      _retryCount = 0;
      return true;
    } catch (e) {
      _stateController.add(ConnectionState.failed);
      return false;
    }
  }

  /// 发送文本消息
  Future<bool> sendText(String text) async {
    if (_socket == null) return false;
    final id = Uuid().v4();
    final msg = jsonEncode({
      'type': 'text',
      'id': id,
      'text': text,
      'ts': DateTime.now().millisecondsSinceEpoch,
    });
    _socket!.writeln(msg);
    // 等待 ack（5 秒超时）
    // 通过 messageStream 监听 ack
  }

  void _startHeartbeat() {
    _heartbeatTimer = Timer.periodic(Duration(seconds: 15), (_) {
      _socket?.writeln('{"type":"ping"}');
      _pongTimer = Timer(Duration(seconds: 10), () {
        // pong 超时，判定断线
        _onDisconnected();
      });
    });
  }

  void _onPong() {
    _pongTimer?.cancel();
  }

  void _onDisconnected() {
    _heartbeatTimer?.cancel();
    _pongTimer?.cancel();
    _socket?.destroy();
    _socket = null;
    if (_retryCount < 5) {
      _stateController.add(ConnectionState.reconnecting);
      _retryCount++;
      final delay = Duration(seconds: 2 << (_retryCount - 1)); // 2,4,8,16,32
      Future.delayed(delay, () {
        if (_ip != null) connect(_ip!, _port!, _code!);
      });
    } else {
      _stateController.add(ConnectionState.failed);
    }
  }

  void disconnect() {
    _heartbeatTimer?.cancel();
    _pongTimer?.cancel();
    _socket?.destroy();
    _socket = null;
    _stateController.add(ConnectionState.disconnected);
  }
}
```

#### 2.2.2 StorageService — 持久化

```dart
import 'package:shared_preferences/shared_preferences.dart';

class PairedPC {
  final String name;
  final String ip;
  final int port;
  final String code;

  PairedPC({required this.name, required this.ip, required this.port, required this.code});

  Map<String, dynamic> toJson() => {'name': name, 'ip': ip, 'port': port, 'code': code};
  factory PairedPC.fromJson(Map<String, dynamic> json) => PairedPC(
    name: json['name'], ip: json['ip'], port: json['port'], code: json['code']);
}

class StorageService {
  static const _key = 'paired_pc';

  static Future<PairedPC?> load() async {
    final prefs = await SharedPreferences.getInstance();
    final str = prefs.getString(_key);
    if (str == null) return null;
    return PairedPC.fromJson(jsonDecode(str));
  }

  static Future<void> save(PairedPC pc) async {
    final prefs = await SharedPreferences.getInstance();
    await prefs.setString(_key, jsonEncode(pc.toJson()));
  }

  static Future<void> clear() async {
    final prefs = await SharedPreferences.getInstance();
    await prefs.remove(_key);
  }
}
```

#### 2.2.3 TextSender — 防抖发送

```dart
class TextSender {
  final TcpService _tcp;
  Timer? _debounceTimer;
  String _lastText = '';

  TextSender(this._tcp);

  /// onChanged 回调
  void onTextChanged(String text) {
    _lastText = text;
    _debounceTimer?.cancel();
    _debounceTimer = Timer(Duration(milliseconds: 300), () {
      _send(text);
    });
  }

  void _send(String text) async {
    final success = await _tcp.sendText(text);
    if (success) {
      // 收到 ack，调用回调清空 TextField
      onSendSuccess?.call();
    } else {
      onSendFail?.call(text);
    }
  }

  VoidCallback? onSendSuccess;
  Function(String)? onSendFail;
}
```

### 2.3 主界面设计

```dart
class HomePage extends StatefulWidget { ... }

class _HomePageState extends State<HomePage> {
  final _tcp = TcpService();
  final _controller = TextEditingController();
  final _focusNode = FocusNode();
  late TextSender _sender;
  ConnectionState _state = ConnectionState.disconnected;
  bool _sending = false;

  @override
  void initState() {
    super.initState();
    _sender = TextSender(_tcp);
    _tcp.stateStream.listen((s) => setState(() => _state = s));
    _tcp.messageStream.listen((msg) {
      if (msg['type'] == 'ack') _handleAck(msg);
      if (msg['type'] == 'pong') _tcp._onPong();
    });
    _autoConnect();
  }

  Future<void> _autoConnect() async {
    final pc = await StorageService.load();
    if (pc != null) {
      _tcp.connect(pc.ip, pc.port, pc.code, deviceName: _deviceName());
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: Column(children: [
        _buildStatusBar(),
        _buildTextField(),
        if (_state != ConnectionState.connected)
          _buildActionArea(),
      ]),
    );
  }

  Widget _buildTextField() {
    final connected = _state == ConnectionState.connected;
    return Padding(
      padding: EdgeInsets.all(16),
      child: TextField(
        controller: _controller,
        focusNode: _focusNode,
        enabled: connected && !_sending,
        maxLines: null,
        textInputAction: TextInputAction.newline,
        autofocus: connected,
        onChanged: _sender.onTextChanged,
        decoration: InputDecoration(
          hintText: connected ? '按住输入法麦克风说话...' : '未连接',
          border: OutlineInputBorder(),
        ),
      ),
    );
  }
}
```

---

## 3. 通信协议完整定义

### 3.1 消息格式规范

**传输层**：TCP 长连接
**编码**：UTF-8
**分隔符**：`\n`（每条消息占一行）
**单行上限**：256KB

### 3.2 消息类型

#### hello（手机 → PC）

```json
{"type":"hello","code":"482916","device":"陈先生的手机"}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| type | string | ✅ | 固定 "hello" |
| code | string | ✅ | 6 位数字配对码 |
| device | string | ✅ | 手机设备名（用于状态展示） |

#### hello_ack（PC → 手机）

```json
{"type":"hello_ack","ok":true,"device":"陈先生的 Deepin"}
```

```json
{"type":"hello_ack","ok":false,"device":"","error":"wrong_code"}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| type | string | ✅ | 固定 "hello_ack" |
| ok | bool | ✅ | 配对是否成功 |
| device | string | ✅ | PC 设备名 |
| error | string | ❌ | 失败原因：`wrong_code` / `cooldown` / `expired` / `already_paired` |

#### text（手机 → PC）

```json
{"type":"text","id":"550e8400-e29b-41d4-a716-446655440000","text":"你好world🌍","ts":1730000000000}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| type | string | ✅ | 固定 "text" |
| id | string | ✅ | UUID v4 |
| text | string | ✅ | UTF-8 文本内容 |
| ts | number | ✅ | 发送时间戳（毫秒） |

#### ack（PC → 手机）

```json
{"type":"ack","id":"550e8400-e29b-41d4-a716-446655440000","ok":true,"clipboard_non_text":false}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| type | string | ✅ | 固定 "ack" |
| id | string | ✅ | 对应 text 消息的 id |
| ok | bool | ✅ | 写入是否成功 |
| clipboard_non_text | bool | ❌ | 剪贴板有非文本内容时 true |
| error | string | ❌ | 失败原因 |

#### ping / pong

```json
{"type":"ping"}
{"type":"pong"}
```

### 3.3 错误码表

| 错误码 | 含义 | 手机端处理 |
|---|---|---|
| `wrong_code` | 配对码错误 | 提示重新扫码 |
| `cooldown` | IP 被冷却 | 提示"请 60 秒后重试" |
| `expired` | 配对码过期 | 提示"请重新展示配对码" |
| `already_paired` | 已有设备连接 | 提示"PC 已连接其他设备" |
| `clipboard_write_failed` | 写入剪贴板失败 | 提示"PC 写入失败，请重试" |

---

## 4. 安全设计

### 4.1 配对码生命周期

```
PC 端启动
  → 读取 config.json
  → if code_hash 不存在 or 已过期:
      → generate_code(): 6 位随机数字
      → code_hash = sha256(code)
      → code_expires_at = now + 5min
      → save config

用户点击"显示配对码"
  → if code 过期: 重新生成
  → QR 码内容: {"ip":"...","port":...,"code":"明文code"}

收到 hello
  → if code_expires_at < now: 回 error="expired"
  → if hash(收到的code) != code_hash: 记录失败, 回 error="wrong_code"
  → if fail_count[ip] >= 5: 回 error="cooldown"
  → else: 配对成功，mark_paired，重新生成 code

配对成功后
  → 重新 generate_code()（下一次配对需新码）
  → 但当前连接继续有效（已建立的长连接不需要再校验 code）
```

### 4.2 网络安全

```
监听范围:
  → 仅绑定局域网网卡 IP（非 0.0.0.0）
  → lan_ip() 遍历网卡找非 loopback 的 IPv4

PC 端无网络外发:
  → TCP Server 只 accept，不主动 connect
  → QR 码生成本地完成，不调用外部 API
  → 剪贴板写入本地完成
  → 无遥测、无崩溃上报、无自动更新检查
```

### 4.3 日志脱敏规范

```rust
// ✅ 正确：只记元信息
log::info!("recv id={} type={} len={}", id, msg_type, text.len());
log::info!("ack sent id={} ok={} non_text={}", id, ok, non_text);

// ❌ 禁止：记录文本内容
log::info!("text content: {}", text);  // 绝对不允许
```

---

## 5. CI/CD 构建方案

### 5.1 仓库结构

```
beetype/
├── desktop/          # Tauri PC 端
│   ├── src-tauri/
│   ├── src/
│   ├── package.json
│   └── Cargo.toml
├── app/      # Flutter 手机端
│   ├── lib/
│   ├── android/
│   ├── ios/
│   └── pubspec.yaml
├── .github/
│   └── workflows/
│       ├── build-pc.yml       # PC 端三平台构建
│       └── build-mobile.yml   # 手机端 APK 构建
└── README.md
```

### 5.2 PC 端 CI（build-pc.yml）

```yaml
name: Build PC

on:
  push:
    tags: ['v*']

jobs:
  build-linux:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust/toolchain@v1
        with: { toolchain: stable }
      - run: cargo install tauri-cli --version "^2.0"
      - run: cd desktop && cargo tauri build
      - uses: actions/upload-artifact@v4
        with: { name: beetype-linux, path: desktop/src-tauri/target/release/bundle/** }

  build-macos:
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust/toolchain@v1
      - run: cargo install tauri-cli --version "^2.0"
      - run: cd desktop && cargo tauri build --target aarch64-apple-darwin
      - uses: actions/upload-artifact@v4
        with: { name: beetype-macos, path: desktop/src-tauri/target/*/release/bundle/** }

  build-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust/toolchain@v1
      - run: cargo install tauri-cli --version "^2.0"
      - run: cd desktop && cargo tauri build
      - uses: actions/upload-artifact@v4
        with: { name: beetype-windows, path: desktop/src-tauri/target/release/bundle/** }
```

### 5.3 手机端 CI（build-mobile.yml）

```yaml
name: Build Mobile

on:
  push:
    tags: ['v*']

jobs:
  build-android:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: subosito/flutter-action@v2
        with: { flutter-version: '3.44.0' }
      - run: cd app && flutter pub get
      - run: cd app && flutter build apk --release
      - uses: actions/upload-artifact@v4
        with: { name: beetype-android, path: app/build/app/outputs/flutter-apk/*.apk }
```

### 5.4 Release 流程

```
1. 本地测试通过
2. git tag v0.1.0
3. git push origin v0.1.0
4. GitHub Actions 自动触发三平台构建
5. 构建产物上传到 GitHub Release
6. 用户下载对应平台安装包
```

---

## 6. 开发排期建议

| 阶段 | 任务 | 依赖 |
|---|---|---|
| Phase 1 | PC 端核心：config + protocol + server + clipboard | 无 |
| Phase 2 | PC 端 UI：tray + QR 码窗口 | Phase 1 |
| Phase 3 | 手机端核心：TcpService + StorageService | Phase 1（协议对齐） |
| Phase 4 | 手机端 UI：HomePage + PairingPage + TextField | Phase 3 |
| Phase 5 | 联调：配对 + 文字发送 + 断线重连 | Phase 2 + 4 |
| Phase 6 | 验收测试：AC-1 ~ AC-30 | Phase 5 |
| Phase 7 | CI/CD + 打包 | Phase 6 |
