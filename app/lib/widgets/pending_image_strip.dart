import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:image_picker/image_picker.dart';

/// 「添加图片」入口按钮——**全 App 只有这一个视觉**：
/// 空态时放在底行（[size] 44），有待发图片时以同样颜色/图标/圆角出现在
/// 缩略图条末尾（[size] 与缩略图齐平）⇒ 用户一眼认出是同一个入口。
///
/// 用 Material 3 的 tonal 观感（浅色底 + 深一档图标）而不是白底裸图标：
/// 裸图标在白色打字区上太浅、看不出是个按钮；tonal 既有明确的按钮感，
/// 又不会像主色实心按钮（发送）那样抢戏。
class AddImageButton extends StatelessWidget {
  final VoidCallback? onPressed;
  final double size;
  final double radius;

  static const IconData icon = Icons.add_photo_alternate_outlined;
  static const String tooltip = '添加图片（选好后和文字一起发送）';

  const AddImageButton({
    super.key,
    required this.onPressed,
    this.size = 44,
    this.radius = 12,
  });

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Tooltip(
      message: tooltip,
      child: Material(
        // 柔和的主题浅色底：明显，但与卡片同族
        color: scheme.primaryContainer,
        borderRadius: BorderRadius.circular(radius),
        child: InkWell(
          borderRadius: BorderRadius.circular(radius),
          onTap: onPressed,
          child: SizedBox(
            width: size,
            height: size,
            child: Icon(icon, size: size * 0.45, color: scheme.onPrimaryContainer),
          ),
        ),
      ),
    );
  }
}

/// 待发图片条：横排缩略图 + 每张右上角「×」+ 末尾「添加图片」入口。
///
/// 缩略图**显式给定尺寸**（不是靠父级约束推）——早先那版把 Stack 直接塞进横向
/// ListView，结果缩略图被压成了两条细缝（用户截图里能看见）。尺寸护栏见
/// `test/pending_image_strip_test.dart`。
class PendingImageStrip extends StatelessWidget {
  final List<XFile> files;
  final ValueChanged<int> onRemove;

  /// 末尾那个「添加图片」按钮（与空态底行的是同一个组件）
  final VoidCallback? onAdd;

  /// 缩略图边长
  static const double thumbSize = 64;

  const PendingImageStrip({
    super.key,
    required this.files,
    required this.onRemove,
    this.onAdd,
  });

  @override
  Widget build(BuildContext context) {
    if (files.isEmpty) return const SizedBox.shrink();
    final scheme = Theme.of(context).colorScheme;
    return SizedBox(
      height: thumbSize,
      child: ListView.separated(
        scrollDirection: Axis.horizontal,
        itemCount: files.length + 1,
        separatorBuilder: (_, __) => const SizedBox(width: 10),
        itemBuilder: (context, i) {
          if (i == files.length) {
            return AddImageButton(onPressed: onAdd, size: thumbSize, radius: 10);
          }
          return SizedBox(
            key: ValueKey('pending-thumb-$i'),
            width: thumbSize,
            height: thumbSize,
            child: Stack(
              children: [
                // 缩略图本体：外面套一层浅底+描边的"瓷砖"，这样即使是白底截图
                // （截图内容大半是白的）也能一眼看出这是一张图，而不是糊在卡片上
                Container(
                  decoration: BoxDecoration(
                    color: scheme.surfaceContainerHighest,
                    borderRadius: BorderRadius.circular(10),
                    border: Border.all(color: scheme.outlineVariant),
                  ),
                  clipBehavior: Clip.antiAlias,
                  child: Image.file(
                    File(files[i].path),
                    width: thumbSize,
                    height: thumbSize,
                    fit: BoxFit.cover,
                    errorBuilder: (_, __, ___) => Container(
                      width: thumbSize,
                      height: thumbSize,
                      color: scheme.surfaceContainerHighest,
                      child: Icon(Icons.broken_image_outlined,
                          size: 22, color: scheme.onSurfaceVariant),
                    ),
                  ),
                ),
                // 「×」压在缩略图右上角内侧：照片深浅不一，所以给它一个半透明底
                Positioned(
                  top: 1,
                  right: 1,
                  child: Material(
                    color: Colors.black54,
                    shape: const CircleBorder(),
                    child: InkWell(
                      customBorder: const CircleBorder(),
                      onTap: () {
                        HapticFeedback.selectionClick();
                        onRemove(i);
                      },
                      child: const Padding(
                        padding: EdgeInsets.all(3),
                        child: Icon(Icons.close_rounded,
                            size: 14, color: Colors.white),
                      ),
                    ),
                  ),
                ),
              ],
            ),
          );
        },
      ),
    );
  }
}
