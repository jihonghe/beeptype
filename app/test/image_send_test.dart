import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';

import 'package:beeptype/services/image_send.dart';

/// 图片发送侧的护栏：分块大小要稳在协议单行上限内，切分必须无损、不丢不重。
void main() {
  test('分块无损：拼回去与原文逐字节相同', () {
    final bytes = Uint8List.fromList(List.generate(1000, (i) => i % 256));
    final chunks = chunkImageBase64(bytes, chunkBytes: 128);
    expect(chunks.length, 8); // 7×128 + 104
    final joined = <int>[];
    for (final c in chunks) {
      joined.addAll(base64Decode(c));
    }
    expect(joined, equals(bytes));
  });

  test('空图片切出空列表（调用方据此直接拒发）', () {
    expect(chunkImageBase64(const []), isEmpty);
  });

  test('每块 base64 后仍小于协议单行上限 256KB', () {
    final bytes = Uint8List(kImageChunkBytes * 3);
    final chunks = chunkImageBase64(bytes);
    expect(chunks.length, 3);
    for (final c in chunks) {
      expect(c.length, lessThan(256 * 1024));
    }
  });

  test('上限判定与电脑端一致（8MB）', () {
    expect(imageTooLarge(kMaxImageBytes), isFalse);
    expect(imageTooLarge(kMaxImageBytes + 1), isTrue);
  });

  test('MIME：jpg/jpeg/其他都按 jpeg 报，png 才报 png', () {
    expect(imageMimeForPath('/tmp/x.jpg'), 'image/jpeg');
    expect(imageMimeForPath('/tmp/X.JPEG'), 'image/jpeg');
    expect(imageMimeForPath('/tmp/a/b.heic'), 'image/jpeg'); // 已经被 picker 转码
    expect(imageMimeForPath('/tmp/shot.PNG'), 'image/png');
  });
}
