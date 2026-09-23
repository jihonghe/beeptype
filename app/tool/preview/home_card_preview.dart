//! **设计稿渲染器（不是测试，不进 CI）**：把"图片相关的 UI"按设计渲染成 PNG，
//! 用来看效果、也用来在改版前后做对照。
//!
//! 跑法（从 `app/` 目录）：
//!
//! ```text
//! flutter test tool/preview/home_card_preview.dart --update-goldens
//! ```
//!
//! 产物在 `tool/preview/goldens/`：`empty.png`（空态）、`attachments.png`（待发 2 张 + 有文字）。
//!
//! 说明：这里**刻意手工搭一份卡片的视觉**（与 `home_page.dart` 的 `_buildEditorCard`
//! 同参数：圆角 20、outlineVariant 描边、padding 16/6/8/6），因为真实页面需要连接状态
//! 与私有字段，测试里注入不了；缩略图条则**直接用真实组件** `PendingImageStrip`，所以
//! 它渲染出来的样子就是线上样子。
import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:image_picker/image_picker.dart';

import 'package:beeptype/widgets/pending_image_strip.dart';

const _fontDir = '/usr/share/fonts/opentype/source-han-cjk';

Future<void> _loadFonts() async {
  for (final entry in {
    'SourceHanSans': 'SourceHanSansSC-Regular.otf',
    'SourceHanSansMedium': 'SourceHanSansSC-Medium.otf',
  }.entries) {
    final file = File('$_fontDir/${entry.value}');
    if (!file.existsSync()) continue;
    final loader = FontLoader(entry.key)
      ..addFont(Future.value(file.readAsBytesSync().buffer.asByteData()));
    await loader.load();
  }
}

/// 生成一张"像照片/截图"的占位图，写到临时文件后包成 XFile
Future<XFile> _fakePhoto(String path, List<Color> colors,
    {bool asScreenshot = false}) async {
  final recorder = ui.PictureRecorder();
  final canvas = Canvas(recorder);
  const w = 300.0;
  const h = 300.0;
  const rect = Rect.fromLTWH(0, 0, w, h);
  canvas.drawRect(
    rect,
    Paint()
      ..shader = ui.Gradient.linear(
        rect.topLeft,
        rect.bottomRight,
        colors,
      ),
  );
  if (asScreenshot) {
    // 画几条"界面"的横线，让它看起来像一张截图
    final paint = Paint()..color = Colors.white.withValues(alpha: 0.85);
    for (var i = 0; i < 5; i++) {
      canvas.drawRRect(
        RRect.fromRectAndRadius(
          Rect.fromLTWH(24, 40 + i * 44, 180 - i * 18, 16),
          const Radius.circular(8),
        ),
        paint,
      );
    }
  }
  final image = await recorder.endRecording().toImage(w.toInt(), h.toInt());
  final data = await image.toByteData(format: ui.ImageByteFormat.png);
  final file = File(path)..writeAsBytesSync(data!.buffer.asUint8List());
  return XFile(file.path);
}

/// 手工复刻的输入卡（视觉参数与线上一致）
Widget _card({
  required ColorScheme scheme,
  required List<XFile> pending,
  required String text,
  required bool showAttachEntry,
}) {
  return Container(
    margin: EdgeInsets.zero,
    decoration: BoxDecoration(
      color: scheme.surface,
      borderRadius: BorderRadius.circular(20),
      border: Border.all(color: scheme.outlineVariant),
    ),
    padding: const EdgeInsets.fromLTRB(16, 6, 8, 6),
    child: Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SizedBox(
          height: 150,
          child: Padding(
            padding: const EdgeInsets.only(top: 12, left: 4),
            child: text.isEmpty
                ? Text(
                    '长按键盘语音键即可语音输入文字',
                    style: TextStyle(
                      fontSize: 18,
                      height: 1.4,
                      color: scheme.onSurfaceVariant.withValues(alpha: 0.5),
                    ),
                  )
                : Text(
                    text,
                    style: TextStyle(
                      fontSize: 18,
                      height: 1.4,
                      color: scheme.onSurface,
                    ),
                  ),
          ),
        ),
        if (pending.isNotEmpty) ...[
          Divider(height: 1, color: scheme.outlineVariant),
          Padding(
            padding: const EdgeInsets.only(top: 10, right: 8),
            child: PendingImageStrip(files: pending, onRemove: (_) {}),
          ),
        ],
        Row(
          children: [
            if (showAttachEntry)
              IconButton(
                onPressed: () {},
                tooltip: '添加图片',
                icon: Icon(Icons.image_outlined,
                    size: 22, color: scheme.onSurfaceVariant),
              ),
            const Spacer(),
            if (text.isNotEmpty || pending.isNotEmpty)
              IconButton.filled(
                onPressed: () {},
                icon: const Icon(Icons.send_rounded, size: 20),
              ),
          ],
        ),
      ],
    ),
  );
}

Widget _phone({required Widget child, required ColorScheme scheme}) => Container(
      width: 393,
      color: scheme.surfaceContainerLowest,
      padding: const EdgeInsets.fromLTRB(16, 24, 16, 24),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          // 顶栏（示意）
          Row(
            children: [
              Text('BeepType',
                  style: TextStyle(
                      fontFamily: 'SourceHanSansMedium',
                      fontSize: 24,
                      fontWeight: FontWeight.w700,
                      color: scheme.onSurface)),
              const Spacer(),
              Icon(Icons.qr_code_2, size: 26, color: scheme.onSurface),
              const SizedBox(width: 18),
              Icon(Icons.settings, size: 26, color: scheme.onSurface),
            ],
          ),
          const SizedBox(height: 18),
          // 状态胶囊（示意）
          Container(
            padding: const EdgeInsets.fromLTRB(16, 14, 16, 14),
            decoration: BoxDecoration(
              color: scheme.surfaceContainerHighest,
              borderRadius: BorderRadius.circular(14),
            ),
            child: Row(
              children: [
                Container(
                  width: 12,
                  height: 12,
                  decoration: const BoxDecoration(
                      color: Color(0xFF2E7D32), shape: BoxShape.circle),
                ),
                const SizedBox(width: 12),
                Text('hjh-pc',
                    style: TextStyle(
                        fontFamily: 'SourceHanSansMedium',
                        fontSize: 17,
                        fontWeight: FontWeight.w600,
                        color: scheme.onSurface)),
                const Spacer(),
                Text('断开',
                    style: TextStyle(fontSize: 17, color: scheme.onSurface)),
              ],
            ),
          ),
          const SizedBox(height: 14),
          child,
        ],
      ),
    );

void main() {
  testWidgets('设计稿：输入卡（空态 / 待发图片）', (tester) async {
    late XFile photoA;
    late XFile photoB;
    late Directory dir;
    // 生成占位图要做真实的引擎异步（toImage/toByteData）：必须放在 runAsync 里，
    // 否则会被测试的假时钟挡住、直接超时
    await tester.runAsync(() async {
      await _loadFonts();
      dir = Directory.systemTemp.createTempSync('bt-preview');
      photoA = await _fakePhoto('${dir.path}/a.png',
          [const Color(0xFF4A90D9), const Color(0xFF1B3B6F)]);
      photoB = await _fakePhoto('${dir.path}/b.png',
          [const Color(0xFFF3F4F6), const Color(0xFFCBD5E1)],
          asScreenshot: true);
    });

    final scheme = ColorScheme.fromSeed(seedColor: const Color(0xFF2C6BED));
    for (final v in [
      ('empty', <XFile>[], ''),
      ('attachments', [photoA, photoB], '帮我把这张图里的报错看一下'),
    ]) {
      await tester.pumpWidget(MaterialApp(
        debugShowCheckedModeBanner: false,
        theme: ThemeData(
          colorScheme: scheme,
          fontFamily: 'SourceHanSans',
          scaffoldBackgroundColor: scheme.surfaceContainerLowest,
        ),
        home: Scaffold(
          body: Center(
            child: _phone(
              scheme: scheme,
              child: _card(
                scheme: scheme,
                pending: v.$2,
                text: v.$3,
                showAttachEntry: v.$2.isEmpty,
              ),
            ),
          ),
        ),
      ));
      await tester.pumpAndSettle();
      // 让 Image.file 的真实文件 IO + 解码跑完，否则缩略图是空框
      await tester.runAsync(() async {
        await Future<void>.delayed(const Duration(milliseconds: 400));
      });
      await tester.pumpAndSettle();
      await expectLater(
        find.byType(Scaffold),
        matchesGoldenFile('goldens/${v.$1}.png'),
      );
    }
    dir.deleteSync(recursive: true);
  });
}
