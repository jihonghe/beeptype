import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:beeptype/pages/settings_page.dart';

/// 设置页的护栏：版本号来自原生通道，读不到时不能崩、也不能显示半截内容。
void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  const channel = MethodChannel('beeptype/device');

  void mockVersion(String? version) {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, (call) async {
      if (call.method == 'getAppVersion') return version;
      return null;
    });
  }

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, null);
  });

  testWidgets('显示名称、slogan、版本号与版权', (tester) async {
    mockVersion('0.5.0');
    await tester.pumpWidget(const MaterialApp(home: SettingsPage()));
    await tester.pumpAndSettle();

    expect(find.text('设置'), findsOneWidget, reason: '页面标题');
    expect(find.text('BeepType'), findsOneWidget);
    expect(find.text('手机说话，电脑落字'), findsOneWidget);
    expect(find.text('Version: 0.5.0'), findsOneWidget);
    expect(find.text('© 2026 jihonghe'), findsOneWidget);
    expect(find.text('关于'), findsNothing, reason: '只有一块内容时不加小标题');
  });

  testWidgets('版本号读不到时只隐藏那一行，其余照常显示', (tester) async {
    mockVersion(null);
    await tester.pumpWidget(const MaterialApp(home: SettingsPage()));
    await tester.pumpAndSettle();

    expect(find.textContaining('Version:'), findsNothing);
    expect(find.text('BeepType'), findsOneWidget);
    expect(find.text('© 2026 jihonghe'), findsOneWidget);
  });
}
