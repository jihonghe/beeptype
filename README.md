<p align="center">
  <strong>BeeType</strong><br>
  <sub>手机说话 · 电脑自动打字</sub>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-blue" alt="Platform">
  <img src="https://img.shields.io/badge/mobile-Android%20%7C%20iOS-green" alt="Mobile">
  <img src="https://img.shields.io/badge/license-MIT-yellow" alt="License">
  <img src="https://img.shields.io/badge/language-Flutter%20%7C%20Rust-orange" alt="Language">
</p>

---

**BeeType** 是一个跨平台工具，把手机上的语音输入无缝搬运到电脑上。手机端长按说话（复用你手机输入法自带的语音识别——豆包、讯飞、微信、系统语音都行），松开后文字自动粘贴到 PC 当前焦点输入框，还可以勾选自动回车发送。

局域网直连、不走云端、零账号、零延迟。

## 它解决什么问题

| 场景 | 之前 | 用 BeeType |
|---|---|---|
| 快速记录想法 | 停下来打字 | 手机说一句话，PC 自动粘贴到编辑器 |
| 远程开会时发消息 | 一只手在键盘一只手在手机 | 说一句话直接发到微信聊天框 |
| Linux 桌面语音输入 | 几乎没有可用方案 | 手机说 → PC 粘贴，复用输入法识别质量 |
| 不想打字 | — | 不想打字 |

## ✨ 功能

- **扫码配对** — PC 显示二维码，手机扫一下就连上（临时配对或绑定主力手机）
- **主力绑定** — 绑定后的手机下次打开自动连接；APP 重装后靠硬件指纹（Android ANDROID_ID / iOS Keychain UUID）恢复绑定，无需重新扫码
- **自动粘贴** — Ctrl+V / Cmd+V 直接把文字送进当前焦点输入框（Linux 终端自动用 Ctrl+Shift+V）
- **自动发送** — 粘贴后自动回车（聊天场景零手动）
- **零账号·零云端** — 局域网 TCP 直连，数据只在手机和 PC 之间，不经任何服务器
- **跨平台** — PC 端 Linux（X11）/ Windows / macOS；手机端 Android（iOS 开发中）

## 🚀 快速开始

### 前置条件

- **PC 端**：Linux（X11 桌面）/ Windows 10+ / macOS 12+
- **手机端**：Android 8.0+（iOS 开发中）
- **网络**：手机和 PC 在同一局域网

### PC 端

#### 方式一：下载预编译版本

前往 [GitHub Releases](https://github.com/jihonghe/beetype/releases) 下载对应平台的安装包：

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
# Linux: webkit2gtk-4.1-dev libayatana-appindicator3-dev xdotool 等
# macOS / Windows: Tauri 文档有一键脚本

cd desktop
npm install  # 或者 pnpm / yarn
npm run tauri build
# 产物在 desktop/src-tauri/target/release/
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

1. 启动 PC 端 BeeType → 系统托盘出现图标，点击打开主窗口
2. 手机启动 BeeType → 点顶部扫码按钮 → 扫 PC 上的二维码
3. （可选）PC 上勾选「记住这台手机」→ 下次手机打开自动连接
4. 手机长按麦克风图标说话 → 松开 → PC 当前焦点处自动粘贴

## 🏗️ 架构概览

```
┌─────────────────┐              TCP + JSON             ┌─────────────────┐
│                 │  ──── 扫码配对码 ──── 6 位码校验 ──── │                 │
│  BeeType App    │  ──── Token 持久化 ────────────────  │  BeeType Desktop │
│  (Flutter)      │  ──── Text payload → Text ack ────  │  (Tauri + Rust)  │
│                 │                                      │                 │
│  mobile_scanner │                                      │  enigo (Ctrl+V)  │
│  shared_prefs   │                                      │  clipboard       │
└─────────────────┘                                      └─────────────────┘
         │                                                        │
  hardware_id                                               hardware_id
  (Android ANDROID_ID                                      (persisted to ~/.config/beetype/
   / iOS Keychain UUID)                                     config.json)
```

| 层 | 协议 | 说明 |
|---|---|---|
| 发现 | UDP 广播 8765 | 手机扫到二维码后可选通过 UDP 发现 PC |
| 配对 | TCP + 6 位随机码 | 码 5 分钟过期，SHA-256 比对 |
| 数据 | TCP + JSON | Text / Ack / Hello / Goodbye |
| 持久 | SharedPreferences (手机) + config.json (PC) | 主力绑定、token、硬件指纹 |

详细设计见 [docs/TECH_DESIGN.md](docs/TECH_DESIGN.md)，产品需求见 [docs/PRD.md](docs/PRD.md)。

## 🧩 平台支持矩阵

| 平台 | PC 端 | 自动粘贴 | 自动发送 | 备注 |
|---|---|---|---|---|
| Linux X11 | ✅ | ✅ | ✅ | 原生支持 |
| Linux Wayland | ✅ 窗口可见 | ❌ | ❌ | Wayland 安全模型禁止后台进程输入注入，回退为纯剪贴板（手动 Ctrl+V） |
| Windows 10+ | ✅ | ✅ | ✅ | |
| macOS 12+ | ✅ | ✅ | ✅ | |
| Android 8.0+ | ✅ | — | — | |
| iOS | 🚧 开发中 | — | — | |

## 🛣️ 路线图

- [ ] iOS 手机端适配（扫码、硬件指纹 Keychain 持久化）
- [ ] Linux Wayland 会话：评估 RemoteDesktop 门户自动授权可行性
- [x] GitHub Release 自动构建（CI）
- [ ] 桌面通知（新手机连接、token 认证失败提示）
- [ ] 历史文本快捷重发（手机端）

## 🤝 贡献

欢迎 Issue 和 PR！详见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 🛡️ 安全

- 所有配对码一次性、5 分钟过期，SHA-256 存储，不存明文
- Token 同样只存 SHA-256 hash
- 局域网直连，数据不经任何外部服务器
- 详见 [SECURITY.md](SECURITY.md)

## 📄 License

[MIT License](LICENSE)

## 🙏 致谢

- [enigo](https://github.com/pasce/enigo) — Rust 跨平台键盘模拟
- [Flutter](https://flutter.dev) — 手机端
- [Tauri](https://tauri.app) — 桌面端
