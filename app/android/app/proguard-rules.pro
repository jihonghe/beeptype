# mobile_scanner 依赖 Google ML Kit Barcode Scanning。
# ML Kit 通过 GMS dynamite 以反射方式动态加载扫码模块，
# R8 full mode 会裁剪/混淆这些类，导致 release 包扫码页
# 相机初始化 NPE（MethodChannel mobile_scanner:
# "Attempt to invoke ... getClass() on a null object reference"）。
# debug 不做压缩所以正常。规则参考 mobile_scanner 官方文档。
-keep class com.google.mlkit.** { *; }
-keep class com.google.android.gms.** { *; }
-dontwarn com.google.mlkit.**
-dontwarn com.google.android.gms.**

# mobile_scanner 插件本体（经 MethodChannel 反射注册）
-keep class dev.steenbakker.mobile_scanner.** { *; }
