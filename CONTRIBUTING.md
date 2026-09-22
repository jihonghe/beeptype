# 贡献指南

欢迎任何形式的贡献！Issue、PR、需求讨论、使用反馈都是改进的动力。

## 行为准则

本项目遵循 [Contributor Covenant](https://www.contributor-covenant.org/) 行为准则。请保持友善、专业、尊重不同意见。

## 开发环境搭建

### 1. 克隆仓库

```bash
git clone https://github.com/jihonghe/beeptype.git
cd beeptype
```

### 2. 系统依赖

**PC 端（Rust + Tauri）**

Tauri 的系统依赖因平台而异，参见官方文档：[Tauri Prerequisites](https://tauri.app/start/prerequisites/)

- Linux (Debian/Ubuntu):
  ```bash
  sudo apt install -y build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev webkit2gtk-4.1-dev
  ```
- macOS / Windows: 参考上面的官方链接安装 Xcode Command Line Tools / Visual Studio Build Tools + WebView2。

安装 Rust:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**手机端（Flutter）**

参考 [Flutter Install](https://docs.flutter.dev/get-started/install) 安装对应平台的 Flutter SDK。建议 3.22+。

### 3. 项目结构

```
beeptype/
├── app/                    # Flutter 手机端（Android + iOS）
│   ├── lib/                # Dart 源码
│   ├── android/            # Android 原生（Kotlin）
│   ├── ios/                # iOS 原生（Swift）
│   └── pubspec.yaml
├── desktop/                # Tauri 桌面端
│   ├── src/                # HTML/CSS/JS
│   └── src-tauri/           # Rust 源码
│       ├── src/
│       ├── Cargo.toml
│       └── tauri.conf.json
├── assets/                 # 图标等静态资源
├── docs/                   # 产品和技术文档
├── README.md
└── LICENSE
```

### 4. 本地开发

同时跑 PC 端和手机端（两台设备连接到同一局域网）：

```bash
# 终端 A：桌面端开发模式
# （前端是 desktop/src/index.html 单文件，没有 npm 工程；改 HTML 后重开窗口即生效）
cd desktop && cargo tauri dev          # 或 cargo build && ./src-tauri/target/debug/beeptype

# 终端 B：手机端开发模式（热重载）
cd app && flutter pub get && flutter run
```

### 5. 构建 Release

```bash
# 桌面端（首次约 10-20 分钟，增量约 2-3 分钟）
cd desktop && cargo tauri build        # 需先装 Tauri CLI：cargo install tauri-cli 或 npm i -g @tauri-apps/cli
# 产物：desktop/src-tauri/target/release/bundle/ 下的 .deb + .AppImage / .dmg / .msi

# Android ARM64 release APK
cd app && flutter build apk --release --target-platform android-arm64
# 产物：app/build/app/outputs/flutter-apk/app-release.apk
```

**版本号规则**：**git tag 是唯一来源**。发布时 CI 会把 tag 版本写进 `app/pubspec.yaml` 和 `desktop/src-tauri/tauri.conf.json`（前者决定 Android 包版本，后者决定安装包版本并被烘焙进二进制，设置页「关于」显示的就是它）。仓库里 `pubspec.yaml` / `tauri.conf.json` / `Cargo.toml` 的三个字面量只服务本地开发构建，保持与最近一次发布一致即可，**不要用 `Cargo.toml` 的版本做展示**（CI 不同步它）。

### 6. 跑起来验证

1. 启动 PC 端 `cargo tauri dev`（或 `cargo build` 后直接跑 `target/debug/beeptype`）→ 托盘图标出现
2. 手机 `flutter run` → 扫码 → 连 PC
3. 发一条短消息，确认自动粘贴 / 自动发送正常
4. 常用设备绑定 → 卸载重装手机 APP → 再扫码 → 确认 hardware_id 恢复常用设备身份
5. 桌面端单测：`cd desktop/src-tauri && cargo test`

## 开发与发布流程（三条轨道）

日常改动、测试包、正式版本是**三件不同的事**，不要混在一起：

| 轨道 | 触发方式 | 做什么 | 成本 |
|---|---|---|---|
| **日常检查** | push 到 `main` / 开 PR | `ci.yml`：`cargo test`（含真实 TCP 的集成测试）+ `flutter analyze` + `flutter test`，**不打包** | 约 2~3 分钟 |
| **测试包** | 维护者在本地构建 | 用官方打包器出 `.deb`：`cd desktop && cargo tauri build --bundles deb --config '{"version":"<下一版本>-test"}'`，产物直接装来验证；**不动仓库里的版本字面量** | 约 8 分钟本机 CPU，0 CI 分钟 |
| **正式版本** | 攒够一批改动后打 tag | 写 `docs/releases/<tag>.md` → 三个版本字面量同步到该版本 → 打 tag 推送 → `release.yml` 全平台构建并发布 Release | 约 22~24 分钟（5 个 runner） |

要点：
- **测试包不改版本号语义**：仓库里 `pubspec.yaml` / `tauri.conf.json` / `Cargo.toml` 的字面量只在正式发版时更新；测试包用 `<下一版本>-test` 这样的版本（`dpkg -i` 能正常覆盖升级，设置页也会显示出来，一眼能看出是测试包）。
- **产品名只有两种写法**：`BeepType`（展示名，出现在界面、文档、包描述里）与 `beeptype`（全小写，用于仓库名、包名、协议 scheme、配置目录、环境变量前缀）。`ci.yml` 有一道 `naming` 检查专门拦旧名：除 `docs/releases/v0.2.2~v0.4.0.md`（历史发布说明，记录的是**当时真实的产品名**，永久不动）与锁文件外，仓库里出现旧名即判失败。
- **不要每改一处就发版**：正式版攒批次发，版本号才不碎、发布说明才好写；期间要验证就用测试包。
- 本地构建 `.deb` 需要 `libayatana-appindicator3-dev`（官方打包器靠 pkg-config 探测 appindicator，缺了会报 `Can't detect any appindicator library`）：`sudo apt install libayatana-appindicator3-dev`。
- 测试包与正式版之间会有一次 dpkg 的"降级"提示：`0.2.5-test` 在 dpkg 的版本序里**大于** `0.2.5`（预发布串没有 `~`，Debian 会把它当成更高的修订号），所以装正式版时会提示一次降级——**正常**，装完就是正式版。想彻底避免就得跳过版本号（测试包用 `0.2.5-test`、正式版发 `0.2.6`），按需取舍。

## 提交流程

1. **Fork 本仓库** → 创建你的功能分支（`feature/xxx` 或 `fix/xxx`）
2. **确保代码可构建**：
   ```bash
   cd app && flutter analyze        # 手机端
   cd desktop/src-tauri && cargo check  # 桌面端
   ```
3. **提交** —— 请遵循 Conventional Commits 风格：
   ```
   feat: 添加 xxx 功能
   fix: 修复 xxx 问题
   refactor: xxx
   docs: xxx
   chore: xxx
   ```
4. **推送到你的 fork** → 提 Pull Request 到 `main` 分支
5. **等待 review** —— 项目维护者会尽快看

## PR Checklist

- [ ] 代码能构建（`flutter analyze` + `cargo check` 无警告）
- [ ] 改动范围聚焦一个问题/功能（大 PR 请拆分）
- [ ] 新功能有对应的文档更新（README / docs/ 下的相关文件）
- [ ] 跨平台改动在测试矩阵覆盖的平台上至少跑过一遍
- [ ] 配置文件变动考虑了老版本的兼容性（serde default 字段、向后兼容的 JSON）

## Issue 模板

- 🐛 Bug 报告 → 使用 `.github/ISSUE_TEMPLATE/bug_report.yml`
- ✨ 功能建议 → 使用 `.github/ISSUE_TEMPLATE/feature_request.yml`

## 设计讨论

较大的产品方向或技术方案，欢迎先开 Issue 讨论再动代码。这能避免实现方向走偏、帮你节省时间。
