<p align="center">
  <strong>BeepType</strong><br>
  <sub>手机说话，电脑落字</sub>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-blue" alt="Platform">
  <img src="https://img.shields.io/badge/mobile-Android-green" alt="Mobile">
  <img src="https://img.shields.io/badge/license-MIT-yellow" alt="License">
  <img src="https://img.shields.io/badge/language-Flutter%20%7C%20Rust-orange" alt="Language">
</p>

---

**BeepType** 是一个跨平台工具，**把你说的话，变成电脑里的字**——手机端长按说话（复用你手机输入法自带的语音识别——豆包、讯飞、微信、系统语音都行），松开后文字自动粘贴到 PC 当前焦点输入框，还可以勾选自动回车发送。

局域网直连、不走云端、零账号、零延迟。

## 它解决什么问题

| 场景 | 之前 | 用 BeepType |
|---|---|---|
| 快速记录想法 | 停下来打字 | 手机说一句话，PC 自动粘贴到编辑器 |
| 远程开会时发消息 | 一只手在键盘一只手在手机 | 说一句话直接发到微信聊天框 |
| Linux 桌面语音输入 | 几乎没有可用方案 | 手机说 → PC 粘贴，复用输入法识别质量 |
| 不想打字 | — | 不想打字 |

## ✨ 功能

- **扫码配对** — PC 显示二维码，手机扫一下就连上（临时配对或绑定常用设备）
- **常用设备绑定** — 绑定后的手机下次打开自动连接；APP 重装后靠硬件指纹（Android ANDROID_ID / iOS Keychain UUID）恢复绑定，无需重新扫码
- **免配重连** — 持长期凭证的手机打开即连，PC 重启、两端换 IP 都不用重新扫码（电脑换 IP 由凭证门控的 UDP 发现自动纠偏）
- **单席位模型** — 一台电脑同一时刻只服务一部手机；常用设备打开 APP 遇到电脑被临时手机占用时显示「电脑使用中」，手动点「接管电脑」才踢人
- **临时手机可转正** — 还没有常用设备时，在 PC 设置里把当前临时设备的手机一键设为常用设备，不用重新扫码
- **临时设备可续连** — 临时手机被系统杀掉后，默认 5 分钟内重开 APP 可免扫码自动连回；电脑换了 IP 也能靠局域网广播找回来（窗口时长可在 PC 设置里关闭/调整）
- **连接失败时有话直说** — 手机和电脑不在同一个网络（用了移动数据、连了别的 Wi-Fi）时会明确提示检查 Wi-Fi；同网却连不上（电脑没开、网络禁止设备互访）则不打扰用户，只静默重试
- **自动粘贴** — Ctrl+V / Cmd+V 直接把文字送进当前焦点输入框（Linux 终端自动用 Ctrl+Shift+V）
- **自动发送** — 粘贴后自动回车（聊天场景零手动）
- **图片传输** — 手机选图（可多选）→ 电脑剪贴板里就是这张图 → 在支持粘贴图片的输入框里自动贴出来
- **零账号·零云端** — 局域网 TCP 直连，数据只在手机和 PC 之间，不经任何服务器
- **一键重启** — 装好新版本后点托盘菜单的「重启应用」即可切换，不必手动退出再打开
- **跨平台** — PC 端 Linux（X11）/ Windows / macOS；手机端 Android（iOS 见路线图）

## 🚀 快速开始

### 前置条件

- **PC 端**：Linux（X11 桌面）/ Windows 10+ / macOS 12+
- **手机端**：Android 8.0+（iOS 未支持，见路线图）
- **网络**：手机和 PC 在同一局域网

### PC 端

#### 方式一：下载预编译版本

前往 [GitHub Releases](https://github.com/jihonghe/beeptype/releases) 下载对应平台的安装包：

| 平台 | 文件 |
|------|------|
| Linux | `.deb` 或 `.AppImage` |
| Windows | `.msi` |
| macOS | `.dmg` |
| Android | `.apk` |

#### 方式二：从源码构建

```bash
# 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 安装 Tauri 依赖（参考 https://tauri.app/start/prerequisites/）
# Linux: libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev 等
#        （与 CI 一致，见 .github/workflows/release.yml）
# macOS / Windows: Tauri 文档有一键脚本

# 安装 Tauri CLI（二选一）
cargo install tauri-cli --version "^2.0"
# 或：npm install -g @tauri-apps/cli@^2.0.0

cd desktop
cargo tauri build
# 产物在 desktop/src-tauri/target/release/bundle/（.deb / .AppImage / .dmg / .msi）
# 开发期可直接 `cargo build` 后运行 target/debug/beeptype
```

### 手机端

```bash
# 安装 Flutter SDK（https://docs.flutter.dev/get-started/install）
cd app
flutter pub get
flutter run         # 开发模式
flutter install --release  # 装 release 包到真机
```

### 使用流程

1. 启动 PC 端 BeepType → 系统托盘出现图标，点击打开主窗口
2. 手机启动 BeepType → 点顶部扫码按钮 → 扫 PC 上的二维码
3. （可选）PC 上勾选「记住这台手机」→ 下次手机打开自动连接
4. 手机长按麦克风图标说话 → 松开 → PC 当前焦点处自动粘贴

## 🏗️ 架构概览

```
┌────────────────────┐        TCP + JSON（行协议）        ┌────────────────────┐
│  BeepType App       │  ── 扫码配对码（一次性随机串）──→   │  BeepType Desktop   │
│  (Flutter)         │  ── Token 持久化（免配重连）───→    │  (Tauri + Rust)    │
│                    │  ── text → ack ───────────────→    │                    │
│  mobile_scanner    │  ←─ session_ended / downgraded ──   │  enigo（按键注入）  │
│  shared_prefs      │                                     │  剪贴板 (arboard)  │
│                    │  ── UDP 发现请求（带 proof）──→     │                    │
│                    │  ←─ 发现应答（仅常用设备可见）─────      │                    │
└────────────────────┘                                     └────────────────────┘
        │                                                          │
  hardware_id                                              device_id / device_name
  (Android ANDROID_ID                                       (首次运行生成 UUIDv4，
   / iOS Keychain UUID)                                      落盘 ~/.config/beeptype/)
```

| 层 | 协议 | 说明 |
|---|---|---|
| 发现 | UDP（与 TCP 同号，默认 8765） | 凭证门控：请求需带 client_id + SHA-256(token) 且与已绑定常用设备一致才单播应答，其余一律静默（对陌生设备隐身）；应答含新 IP 与 busy 位 |
| 配对 | TCP + 一次性配对码 | 码为 64 位 hex 随机串（32 字节 /dev/urandom），5 分钟过期、只随二维码下发（无手输入口）、配对成功后立即换新，SHA-256 比对 |
| 认证 | TCP + token | 绑定常用设备时一次性下发明文 token，两端只存 SHA-256；此后打开 APP 凭 token 免码直连 |
| 数据 | TCP + JSON 行协议 | hello / hello_ack / text / ack / ping / pong / takeover / bye / session_ended / session_downgraded，单行上限 256KB |
| 持久 | SharedPreferences (手机) + config.json (PC) | 手机存 client_id 与已绑定电脑列表（含 token）；PC 存本机身份、唯一常用设备记录、配对码 hash；临时会话两端都不落盘 |

详细设计见 [docs/TECH_DESIGN.md](docs/TECH_DESIGN.md)，产品需求见 [docs/PRD.md](docs/PRD.md)。

## 🧩 平台支持矩阵

| 平台 | PC 端 | 自动粘贴 | 自动发送 | 备注 |
|---|---|---|---|---|
| Linux X11 | ✅ | ✅ | ✅ | 原生支持 |
| Linux Wayland | ✅ 窗口可见 | ❌ | ❌ | Wayland 安全模型禁止后台进程输入注入，回退为纯剪贴板（手动 Ctrl+V） |
| Windows 10+ | ✅ | ✅ | ✅ | |
| macOS 12+ | ✅ | ✅ | ✅ | |
| Android 8.0+ | ✅ | — | — | |
| iOS | ⚪ 未支持 | — | — | 原生通道代码已就绪（Keychain 硬件指纹等），尚未在真机验证、也还没有分发 |

## 🛣️ 路线图

- [ ] **发送延迟** — 吸收分段型输入法（搜狗/百度会在说话过程中逐段提交），让回车只在整段说完后触发一次，而不是把一整段打成碎片
- [ ] **图片传输** — 手机选图 → 电脑剪贴板里的图片 → 在聊天框 Ctrl+V 直达（可配合自动粘贴）
- [ ] **手机端输入控制台** — 历史记录、编辑后重发、失败不丢原文
- [ ] **iOS 适配** — 原生通道代码已就绪（Keychain 硬件指纹、设备名、版本号），待真机验证与分发
- [ ] **Linux Wayland 会话的自动粘贴**（探路）— 触发条件：自己的会话切到 Wayland，或出现真实用户需求
- [x] GitHub Release 自动构建（CI）

> 产品定位、需求分层、竞品地图与明确**不做**的事，见 [`docs/PRODUCT.md`](docs/PRODUCT.md)。

## 🤝 贡献

欢迎 Issue 和 PR！详见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 🛡️ 安全

- 配对码为 32 字节随机串（64 位 hex），只随二维码出现、5 分钟过期、配对成功即换新，PC 端只存 SHA-256
- 常用设备 token 同样只存 SHA-256；UDP 发现的 proof 用的就是 SHA-256(token)，抓到广播也无法用于 TCP 认证
- 同一 IP 连续 5 次鉴权失败冷却 60 秒
- 局域网直连，数据不经任何外部服务器
- 详见 [SECURITY.md](SECURITY.md)

## 📄 License

[MIT License](LICENSE)

## 🙏 致谢

- [enigo](https://github.com/pasce/enigo) — Rust 跨平台键盘模拟
- [Flutter](https://flutter.dev) — 手机端
- [Tauri](https://tauri.app) — 桌面端
