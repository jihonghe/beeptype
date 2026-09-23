import 'dart:convert';

/// 单张图片上限（与电脑端 `image_receiver.rs` 的 `MAX_IMAGE_BYTES` 一致）
const int kMaxImageBytes = 8 * 1024 * 1024;

/// 每块原始字节数。base64 后约 ×4/3 ⇒ 135KB → 180KB，
/// 稳在协议单行上限（256KB）之内。
const int kImageChunkBytes = 135 * 1024;

/// 把整张图片切成 base64 分块（最后一块可能更小）。
///
/// 为什么不用一条消息发整张：单行上限 256KB，且 base64 会让内存翻倍。
List<String> chunkImageBase64(List<int> bytes, {int chunkBytes = kImageChunkBytes}) {
  if (bytes.isEmpty) return const [];
  final out = <String>[];
  for (var start = 0; start < bytes.length; start += chunkBytes) {
    final end = (start + chunkBytes < bytes.length) ? start + chunkBytes : bytes.length;
    out.add(base64Encode(bytes.sublist(start, end)));
  }
  return out;
}

/// 是否超过单张上限
bool imageTooLarge(int byteLength) => byteLength > kMaxImageBytes;

/// 按文件后缀猜 MIME。
///
/// 手机端统一用 `image_picker` 的 `imageQuality` 让系统重编码成 **JPEG**
/// （一举解决 HEIC/WebP 这类电脑端解不开的格式，并顺带压体积），
/// 所以正常情况这里都会拿到 `.jpg`；PNG 分支留给"将来支持截图原样传"。
String imageMimeForPath(String path) {
  final p = path.toLowerCase();
  if (p.endsWith('.png')) return 'image/png';
  return 'image/jpeg';
}

/// 有没有东西可发（用于决定"发送"按钮是否出现）：
/// 有文字，或攒着待发的图片，都算。
bool hasSomethingToSend({required bool hasText, required int pendingImages}) =>
    hasText || pendingImages > 0;
