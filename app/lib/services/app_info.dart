import 'package:flutter/services.dart';

/// APP 自身信息（关于页用）。
///
/// 走与设备名/硬件指纹同一条自建通道，不引入三方插件：Android 读 versionName、
/// iOS 读 CFBundleShortVersionString。两者都由发布流程写入（CI 打 tag 时同步
/// `app/pubspec.yaml` 的 version），所以关于页显示的版本号与安装包一致。
class AppInfo {
  static const _channel = MethodChannel('beeptype/device');

  /// 版本号（如 `0.5.0`）。取不到时返回空串，界面自行隐藏该行。
  static Future<String> version() async {
    try {
      final v = await _channel.invokeMethod<String>('getAppVersion');
      return v?.trim() ?? '';
    } catch (_) {
      // 通道不可用（非安卓/iOS 平台、引擎未就绪）时按"读不到"处理
      return '';
    }
  }
}
