import 'package:flutter/material.dart';

import '../services/app_info.dart';

/// 设置页：今天的内容就是 APP 信息（图标 + 名称 + slogan + 版本号 + 版权）。
///
/// 从首页右上角的齿轮一步进入。APP 目前没有设置项，所以这一页只做展示、不放
/// 任何可操作的东西；**将来真有设置项就加在这块信息上面**（与 LocalSend 的设置页
/// 同一种排版：上面设置项、下面 APP 信息），入口与页面名字都不用改。
/// 今天不给信息块加小标题：小标题是用来分隔多组内容的，只有一块内容时反而突兀。
class SettingsPage extends StatefulWidget {
  const SettingsPage({super.key});

  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  String _version = '';

  @override
  void initState() {
    super.initState();
    AppInfo.version().then((v) {
      if (mounted && v.isNotEmpty) setState(() => _version = v);
    });
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final muted = TextStyle(fontSize: 13, color: scheme.onSurfaceVariant);
    return Scaffold(
      appBar: AppBar(title: const Text('设置')),
      body: SafeArea(
        child: Center(
          child: SingleChildScrollView(
            padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 24),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                // 与应用图标同源（scripts/build_icons.py 从母版生成），不额外维护第二份设计
                Image.asset(
                  'assets/app_icon.png',
                  width: 128,
                  height: 128,
                  errorBuilder: (_, __, ___) =>
                      const SizedBox(width: 128, height: 128),
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
            ),
          ),
        ),
      ),
    );
  }
}
