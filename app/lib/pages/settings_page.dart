import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../services/app_info.dart';
import '../services/text_sender.dart';
import '../services/prefs_policy.dart';
import '../services/tcp_service.dart';

/// 设置页：**电脑端设置**（行为开关）+ APP 信息。
///
/// 从首页右上角的齿轮一步进入。行为开关改的是**电脑**的行为（粘贴/回车），
/// 所以文案与电脑端设置页逐字一致，并且**只对常用设备身份显示**——电脑端也只接受
/// 常用设备席位的请求，临时访客改了不生效，索性不给它看。
///
/// 将来电脑端还有别的设置项，都放进「电脑端设置」这一块，放在 APP 信息上面
/// （与 LocalSend 的设置页同一种排版）。
class SettingsPage extends StatefulWidget {
  final TextSender sender;
  const SettingsPage({super.key, required this.sender});

  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  String _version = '';
  StreamSubscription<PrefsState>? _prefsSub;
  StreamSubscription<void>? _sessionSub;

  @override
  void initState() {
    super.initState();
    AppInfo.version().then((v) {
      if (mounted && v.isNotEmpty) setState(() => _version = v);
    });
    // 只收起键盘、**不要手动 unfocus**：推路由本身已经让输入框失焦，而手动 unfocus
    // 会把"这一层上次聚焦的是谁"的记忆一起清掉 → 返回首页时光标回不来（用户实测的
    // "键盘弹了但光标不在"就是它）。返回首页时由首页重新聚焦。
    WidgetsBinding.instance.addPostFrameCallback((_) {
      SystemChannels.textInput.invokeMethod<void>('TextInput.hide');
    });
    // 行为开关的值来自电脑端推送；身份变化（常用↔临时）也决定这一块显不显示
    _prefsSub = widget.sender.prefsStream.listen((_) {
      if (mounted) setState(() {});
    });
    _sessionSub = widget.sender.sessionUpdated.listen((_) {
      if (mounted) setState(() {});
    });
  }

  @override
  void dispose() {
    _prefsSub?.cancel();
    _sessionSub?.cancel();
    super.dispose();
  }

  /// 「电脑端设置」这一块只在**当前这条连接是常用设备身份**时出现。
  ///
  /// 临时访客不显示（改不了）；还没从电脑端同步到值之前也不显示，避免闪一下错状态。
  bool get _showPcSettings => showPcSettings(
        hasPrefs: widget.sender.prefs != null,
        sessionOk: widget.sender.session?.ok,
        sessionIsPrimary: widget.sender.session?.isPrimary,
      );

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final info = _appInfo(scheme);

    return Scaffold(
      appBar: AppBar(title: const Text('设置')),
      body: SafeArea(
        // 一套布局，两种状态一致：主内容在上、APP 信息**始终贴页面底部**。
        // 屏幕够高就把内容撑到整屏（好让信息贴底）；内容真超一屏时按自然高度滚动，
        // 不挤压内容（小屏/大字体场景）。
        child: LayoutBuilder(
          builder: (context, constraints) => SingleChildScrollView(
            child: ConstrainedBox(
              constraints: BoxConstraints(minHeight: constraints.maxHeight),
              child: Padding(
                padding: const EdgeInsets.fromLTRB(16, 14, 16, 20),
                child: Column(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  // 铺满宽度：否则 Column 的宽度由"最宽的子项"决定，
                  // 没有设置区时（最宽子项就是信息块自己）整块会被摆到左边（用户实测）
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    if (_showPcSettings)
                      _pcSettings(scheme)
                    else
                      const SizedBox.shrink(),
                    Padding(
                      // 与上方内容至少留出这么多，贴底时不至于挤在一起
                      padding: const EdgeInsets.only(top: 28),
                      child: info,
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  /// 电脑端设置区：文案与电脑端设置页逐字一致（标签 + 说明），
  /// 让用户在手机上也能一眼看出"这是电脑端的设置"。
  Widget _pcSettings(ColorScheme scheme) {
    final prefs = widget.sender.prefs!;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          '电脑端设置',
          style: TextStyle(
            fontSize: 13,
            fontWeight: FontWeight.w600,
            color: scheme.primary,
          ),
        ),
        const SizedBox(height: 6),
        Card(
          margin: EdgeInsets.zero,
          elevation: 0,
          color: scheme.surface,
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(16),
            side: BorderSide(color: scheme.outlineVariant),
          ),
          child: Padding(
            padding: const EdgeInsets.fromLTRB(14, 8, 6, 8),
            child: Column(
              children: [
                _prefsRow(
                  scheme,
                  title: '接收后自动粘贴',
                  note: '模拟快捷键把剪贴板内容粘贴到当前光标处',
                  value: prefs.autoPaste,
                  onChanged: (v) => widget.sender.setPrefs(autoPaste: v),
                ),
                Divider(height: 1, color: scheme.outlineVariant),
                _prefsRow(
                  scheme,
                  title: '粘贴后自动发送',
                  // 粘贴关着时说明变成「请先开启自动粘贴」（与电脑端一字不差），
                  // 免得用户只看到一个莫名其妙的置灰开关
                  note: prefs.autoPaste
                      ? '粘贴完成后自动回车发送到聊天框'
                      : '请先开启自动粘贴',
                  value: prefs.autoSend,
                  enabled: prefs.autoPaste,
                  onChanged: (v) => widget.sender.setPrefs(autoSend: v),
                ),
              ],
            ),
          ),
        ),
      ],
    );
  }

  Widget _prefsRow(
    ColorScheme scheme, {
    required String title,
    required String note,
    required bool value,
    required ValueChanged<bool> onChanged,
    bool enabled = true,
  }) {
    final dim = !enabled;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  title,
                  style: TextStyle(
                    fontSize: 15,
                    fontWeight: FontWeight.w600,
                    color: dim
                        ? scheme.onSurfaceVariant.withValues(alpha: 0.5)
                        : scheme.onSurface,
                  ),
                ),
                const SizedBox(height: 3),
                Text(
                  note,
                  style: TextStyle(
                    fontSize: 12,
                    color: scheme.onSurfaceVariant
                        .withValues(alpha: dim ? 0.4 : 0.8),
                  ),
                ),
              ],
            ),
          ),
          const SizedBox(width: 8),
          Switch(
            value: value,
            onChanged: enabled ? onChanged : null,
            materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
          ),
        ],
      ),
    );
  }

  /// APP 信息块（与 LocalSend 一样不加小标题，靠位置与间距分隔）
  Widget _appInfo(ColorScheme scheme) {
    final muted = TextStyle(fontSize: 13, color: scheme.onSurfaceVariant);
    return Column(
      mainAxisSize: MainAxisSize.min,
      children: [
        Image.asset(
          'assets/app_icon.png',
          width: 128,
          height: 128,
          errorBuilder: (_, __, ___) => const SizedBox(width: 128, height: 128),
        ),
        const SizedBox(height: 22),
        const Text(
          'BeepType',
          style: TextStyle(fontSize: 26, fontWeight: FontWeight.w700),
        ),
        const SizedBox(height: 8),
        Text('手机说话，电脑落字',
            style: TextStyle(fontSize: 14, color: scheme.onSurfaceVariant)),
        const SizedBox(height: 18),
        if (_version.isNotEmpty) Text('Version: $_version', style: muted),
        const SizedBox(height: 4),
        Text('© 2026 jihonghe', style: muted),
      ],
    );
  }
}
