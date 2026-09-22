import 'package:flutter/material.dart';
import 'services/text_sender.dart';
import 'pages/home_page.dart';
import 'pages/scan_page.dart';
import 'pages/settings_page.dart';

Future<void> main() async {
  // 必须在 runApp 前完成绑定与存储初始化，否则 HomePage 冷启动
  // 立即调用 autoConnect() 会访问尚未初始化的 SharedPreferences 而崩溃
  WidgetsFlutterBinding.ensureInitialized();
  final sender = TextSender();
  await sender.init();
  runApp(BeepTypeApp(sender: sender));
}

class BeepTypeApp extends StatelessWidget {
  final TextSender sender;

  const BeepTypeApp({super.key, required this.sender});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'BeepType',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.blue),
        useMaterial3: true,
      ),
      home: HomePage(sender: sender),
      routes: {
        '/scan': (context) => ScanPage(sender: sender),
        '/settings': (context) => SettingsPage(sender: sender),
      },
    );
  }
}
