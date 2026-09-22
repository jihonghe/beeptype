import Flutter
import UIKit
import Security

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  /// 缓存 engine 的 binaryMessenger，供 setupBeepTypeChannel 用
  private var _binaryMessenger: FlutterBinaryMessenger?

  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    let ok = super.application(application, didFinishLaunchingWithOptions: launchOptions)
    // 从根控制器拿 messenger（Flutter 传统方式，Implicit Engine 下也工作）
    if let controller = window?.rootViewController as? FlutterViewController {
      _binaryMessenger = controller.binaryMessenger
      setupBeepTypeChannel(messenger: controller.binaryMessenger)
    }
    return ok
  }

  func didInitializeImplicitFlutterEngine(_ engineBridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: engineBridge.pluginRegistry)
    // Implicit Engine 模式下也要注册我们的通道，用 bridge 的 messenger
    setupBeepTypeChannel(messenger: engineBridge.messenger)
  }

  private func setupBeepTypeChannel(messenger: FlutterBinaryMessenger) {
    let channel = FlutterMethodChannel(
      name: "beeptype/device",
      binaryMessenger: messenger
    )
    channel.setMethodCallHandler(handleDeviceMethodCall)
  }

  private func handleDeviceMethodCall(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    switch call.method {
    case "getDeviceName":
      result(UIDevice.current.name)
    case "getHardwareId":
      result(hardwareId())
    case "getAppVersion":
      // 关于页显示的版本号：与 pubspec.yaml 的 version 一致（发布时由 CI 按 tag 改写）
      result(Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String)
    default:
      result(FlutterMethodNotImplemented)
    }
  }

  // MARK: - Hardware ID

  /// 硬件指纹：Keychain 持久化的 UUID。
  /// Keychain 独立于 APP 沙箱——同 bundle ID APP 卸载重装后数据仍然存在，
  /// 效果等价于 Android ANDROID_ID。恢复出厂设置才会清空。
  ///
  /// 流程：
  /// 1. 读 Keychain（service = "com.beeptype.hardware-id"）
  /// 2. 读到 → 返回；没读到 → 生成新 UUID → 存 Keychain → 返回
  private func hardwareId() -> String {
    let service = "com.beeptype.hardware-id"

    // 1. 读
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecReturnData as String: true,
      kSecMatchLimit as String: kSecMatchLimitOne,
    ]
    var item: AnyObject?
    if SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
       let data = item as? Data,
       let uuid = String(data: data, encoding: .utf8),
       !uuid.isEmpty {
      return uuid
    }

    // 2. 生成 + 存
    let newUuid = UUID().uuidString
    let addItem: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword,
      kSecAttrService as String: service,
      kSecValueData as String: newUuid.data(using: .utf8)!,
      // 解锁后可访问——APP 正常启动、解锁状态下都满足
      kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock,
    ]
    // add 失败静默忽略（系统不允许时返回 nil，Dart 端会 fallback）
    _ = SecItemAdd(addItem as CFDictionary, nil)

    return newUuid
  }
}
