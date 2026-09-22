import 'package:flutter/material.dart';

import '../services/app_info.dart';

/// 关于页：图标 + 名称 + slogan + 版本号 + 版权。
///
/// 从首页右上角「⋮」进入。APP 目前没有设置项，所以这一页只做展示、
/// 不放任何可操作的东西；将来真有设置项，入口也挂在同一个「⋮」里。
class AboutPage extends StatefulWidget {
  const AboutPage({super.key});

  @override
  State<AboutPage> createState() => _AboutPageState();
}

class _AboutPageState extends State<AboutPage> {
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
      appBar: AppBar(title: const Text('关于')),
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
