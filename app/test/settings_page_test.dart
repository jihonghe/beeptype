import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:beeptype/pages/settings_page.dart';
import 'package:beeptype/services/text_sender.dart';

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
    await tester.pumpWidget(MaterialApp(home: SettingsPage(sender: TextSender())));
    await tester.pumpAndSettle();

    expect(find.text('设置'), findsOneWidget, reason: '页面标题');
    expect(find.text('BeepType'), findsOneWidget);
    expect(find.text('手机说话，电脑落字'), findsOneWidget);
    expect(find.text('Version: 0.5.0'), findsOneWidget);
    expect(find.text('© 2026 jihonghe'), findsOneWidget);
    expect(find.text('关于'), findsNothing, reason: '只有一块内容时不加小标题');
  });

  testWidgets('没有「电脑端设置」时，APP 信息块水平居中且贴底', (tester) async {
    mockVersion('0.7.0');
    await tester.pumpWidget(
        MaterialApp(home: SettingsPage(sender: TextSender())));
    await tester.pumpAndSettle();

    // 回归护栏（用户实测）：外层 Column 若没铺满宽度，宽度就由"最宽的子项"决定——
    // 没有设置区时最宽子项就是信息块自己，于是整块被摆到左边（看起来贴在左下角）。
    final w = tester.view.physicalSize.width / tester.view.devicePixelRatio;
    final h = tester.view.physicalSize.height / tester.view.devicePixelRatio;
    expect(tester.getCenter(find.text('BeepType')).dx,
        moreOrLessEquals(w / 2, epsilon: 1));
    expect(tester.getCenter(find.byType(Image)).dx,
        moreOrLessEquals(w / 2, epsilon: 1));
    // 同时确认它仍然贴着页面底部（spaceBetween + 最小高度约束）
    expect(tester.getBottomRight(find.text('© 2026 jihonghe')).dy,
        greaterThan(h * 0.8));
  });

  testWidgets('版本号读不到时只隐藏那一行，其余照常显示', (tester) async {
    mockVersion(null);
    await tester.pumpWidget(MaterialApp(home: SettingsPage(sender: TextSender())));
    await tester.pumpAndSettle();

    expect(find.textContaining('Version:'), findsNothing);
    expect(find.text('BeepType'), findsOneWidget);
    expect(find.text('© 2026 jihonghe'), findsOneWidget);
  });
}
