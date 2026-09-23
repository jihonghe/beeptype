import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:image_picker/image_picker.dart';

import 'package:beeptype/widgets/pending_image_strip.dart';

/// 待发图片条的护栏：缩略图**必须有正常尺寸**——早先那版被压成了两条细缝
/// （用户截图里看得见），所以这里直接量尺寸：宽高都得等于 thumbSize。
void main() {
  // 1×1 的透明 PNG，够 Image.file 解码出真实尺寸
  final pngBytes = Uint8List.fromList(base64Decode(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk'
    'YPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  ));

  late Directory dir;
  setUp(() => dir = Directory.systemTemp.createTempSync('bt-strip'));
  tearDown(() => dir.deleteSync(recursive: true));

  XFile makePng(String name) {
    final f = File('${dir.path}/$name.png')..writeAsBytesSync(pngBytes);
    return XFile(f.path);
  }

  testWidgets('缩略图尺寸正常，且每张都能点到「×」', (tester) async {
    final files = [makePng('a'), makePng('b')];
    final removed = <int>[];
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: Align(
          alignment: Alignment.bottomCenter,
          child: PendingImageStrip(files: files, onRemove: removed.add),
        ),
      ),
    ));
    await tester.pumpAndSettle();

    // 关键断言：缩略图不是细缝
    final images = find.byType(Image);
    expect(images, findsNWidgets(2));
    for (var i = 0; i < 2; i++) {
      final size = tester.getSize(images.at(i));
      expect(size.width, moreOrLessEquals(PendingImageStrip.thumbSize));
      expect(size.height, moreOrLessEquals(PendingImageStrip.thumbSize));
    }

    // 两个「×」都在，点了能回调到对应的下标
    final closeIcons = find.byIcon(Icons.close_rounded);
    expect(closeIcons, findsNWidgets(2));
    await tester.tap(closeIcons.first);
    expect(removed, [0]);
  });

  testWidgets('末尾就是「添加图片」入口（与空态底行同一个组件）', (tester) async {
    var addTaps = 0;
    await tester.pumpWidget(MaterialApp(
      theme: ThemeData(colorScheme: ColorScheme.fromSeed(seedColor: Colors.blue)),
      home: Scaffold(
        body: Align(
          alignment: Alignment.bottomCenter,
          child: PendingImageStrip(
            files: [makePng('a')],
            onRemove: (_) {},
            onAdd: () => addTaps++,
          ),
        ),
      ),
    ));
    await tester.pumpAndSettle();

    final add = find.byType(AddImageButton);
    expect(add, findsOneWidget, reason: '缩略图条末尾必须有"添加图片"入口');
    // 与缩略图齐平：尺寸一致，用户一眼认出是同一个入口
    final addSize = tester.getSize(add);
    expect(addSize.width, moreOrLessEquals(PendingImageStrip.thumbSize));
    expect(addSize.height, moreOrLessEquals(PendingImageStrip.thumbSize));
    await tester.tap(add);
    expect(addTaps, 1);
  });

  testWidgets('没有待发图片时整条不占位', (tester) async {
    await tester.pumpWidget(MaterialApp(
      home: Scaffold(body: PendingImageStrip(files: const [], onRemove: (_) {})),
    ));
    expect(tester.getSize(find.byType(PendingImageStrip)).height, 0);
  });
}
