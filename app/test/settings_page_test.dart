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

  testWidgets('进设置页再返回后，原页面的输入框能拿回焦点（别手动 unfocus）', (tester) async {
    mockVersion('0.7.0');
    final node = FocusNode();
    final nav = GlobalKey<NavigatorState>();
    await tester.pumpWidget(MaterialApp(
      navigatorKey: nav,
      home: Scaffold(
        body: Column(
          children: [
            TextField(focusNode: node),
            TextButton(
              onPressed: () => Navigator.of(nav.currentContext!).push(
                MaterialPageRoute(
                    builder: (_) => SettingsPage(sender: TextSender())),
              ),
              child: const Text('去设置'),
            ),
          ],
        ),
      ),
    ));
    await tester.pumpAndSettle();
    node.requestFocus();
    await tester.pumpAndSettle();
    expect(node.hasFocus, isTrue);

    await tester.tap(find.text('去设置'));
    await tester.pumpAndSettle();
    expect(node.hasFocus, isFalse, reason: '推路由本身就会让输入框失焦');

    nav.currentState!.pop();
    await tester.pumpAndSettle();
    // 回归护栏（用户实测）：设置页若手动 unfocus，会把"这层上次聚焦的是谁"的记忆
    // 一起清掉，返回后焦点回不来——表现为"键盘弹了但光标不在"。
    expect(node.hasFocus, isTrue, reason: '返回后应自动恢复焦点');
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

  testWidgets('键盘还开着进来时，信息块首帧就贴底；键盘收起也不再滑动', (tester) async {
    mockVersion('0.7.0');
    addTearDown(tester.view.reset);
    // 从打字区点齿轮进设置页时键盘还开着：首帧就带着键盘 inset（900 物理 / 3 dpr = 300 逻辑）
    tester.view.viewInsets = const FakeViewPadding(bottom: 900);

    await tester.pumpWidget(MaterialApp(home: SettingsPage(sender: TextSender())));
    await tester.pump();
    final h = tester.view.physicalSize.height / tester.view.devicePixelRatio;
    final bottom0 = tester.getBottomRight(find.text('© 2026 jihonghe')).dy;
    // 回归护栏（用户实测）：Scaffold 若跟着键盘缩，首帧就按"屏幕−键盘"这个矮高度排版，
    // 信息块落在屏幕中间，键盘一收再往下滑一下 = "信息块从中间滑到底部"。
    expect(bottom0, greaterThan(h * 0.8), reason: '键盘开着时也必须已经在底部');

    // 键盘收起（进设置页会收起键盘）→ 位置一格都不许动
    tester.view.viewInsets = FakeViewPadding.zero;
    await tester.pumpAndSettle();
    expect(tester.getBottomRight(find.text('© 2026 jihonghe')).dy,
        moreOrLessEquals(bottom0, epsilon: 0.5), reason: '键盘收起不能让信息块再滑一下');
  });
}
