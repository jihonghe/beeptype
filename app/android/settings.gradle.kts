pluginManagement {
    val flutterSdkPath =
        run {
            val properties = java.util.Properties()
            file("local.properties").inputStream().use { properties.load(it) }
            val flutterSdkPath = properties.getProperty("flutter.sdk")
            require(flutterSdkPath != null) { "flutter.sdk not set in local.properties" }
            flutterSdkPath
        }

    includeBuild("$flutterSdkPath/packages/flutter_tools/gradle")

    repositories {
        // 国内镜像（阿里云），官方源作为兜底
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/public")
        maven("https://maven.aliyun.com/repository/gradle-plugin")
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

plugins {
    id("dev.flutter.flutter-plugin-loader") version "1.0.0"
    id("com.android.application") version "9.1.0" apply false
    id("org.jetbrains.kotlin.android") version "2.4.0" apply false
}

// Flutter 引擎制品（io.flutter:arm64_v8a_debug 等，单包 100MB+）镜像。
// Flutter Gradle 插件在 apply 时会注入 storage.googleapis.com/download.flutter.io，
// 国内走代理仅约 50KB/s；在此（project evaluate 之前）抢先注册国内镜像，
// Gradle 按仓库顺序解析时先命中镜像。不依赖 FLUTTER_STORAGE_BASE_URL 环境变量。
gradle.allprojects {
    repositories {
        maven { url = uri("https://storage.flutter-io.cn/download.flutter.io") }
    }
}

include(":app")
