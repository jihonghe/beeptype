# 贡献指南

欢迎任何形式的贡献！Issue、PR、需求讨论、使用反馈都是改进的动力。

## 行为准则

本项目遵循 [Contributor Covenant](https://www.contributor-covenant.org/) 行为准则。请保持友善、专业、尊重不同意见。

## 开发环境搭建

### 1. 克隆仓库

```bash
git clone https://github.com/jihonghe/beetype.git
cd beetype
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
beetype/
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
# 终端 A：桌面端开发模式（热重载）
cd desktop && npm install && npm run tauri dev

# 终端 B：手机端开发模式（热重载）
cd app && flutter pub get && flutter run
```

### 5. 构建 Release

```bash
# 桌面端（首次约 10-20 分钟，增量约 2-3 分钟）
cd desktop && npm run tauri build
# 产物：desktop/src-tauri/target/release/beetype (Linux) / .dmg (macOS) / .msi (Windows)

# Android ARM64 release APK
cd app && flutter build apk --release --target-platform android-arm64
# 产物：app/build/app/outputs/flutter-apk/app-release.apk
```

### 6. 跑起来验证

1. 启动 PC 端 `tauri dev` → 托盘图标出现
2. 手机 `flutter run` → 扫码 → 连 PC
3. 发一条短消息，确认 auto_paste / auto_send 正常
4. 主力绑定 → 卸载重装手机 APP → 再扫码 → 确认 hardware_id 恢复主力

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
