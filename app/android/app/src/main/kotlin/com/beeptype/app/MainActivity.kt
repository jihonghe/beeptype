package com.beeptype.app

import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioManager
import android.os.Build
import android.provider.Settings
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "beeptype/device")
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "getDeviceName" -> result.success(deviceName())
                    "getHardwareId" -> result.success(hardwareId())
                    "getAppVersion" -> result.success(appVersion())
                    "getActiveRecordingCount" -> result.success(activeRecordingCount())
                    else -> result.notImplemented()
                }
            }
    }

    /**
     * 临时诊断（验完删）：当前有多少个"正在录音"的音频配置。
     *
     * 用来验证能否用"麦克风被占用"代理"输入法正在语音输入"——语音输入必然占用麦克风，
     * 录音结束≈用户松手。**注意**：Android 10+ 对其他应用的记录配置做了匿名化/隐藏，
     * 返回恒为 0 也是有效结论（那说明这条路不通，只能靠 insets 或显式收尾）。
     */
    private fun activeRecordingCount(): Int = try {
        val am = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        am.activeRecordingConfigurations.size
    } catch (_: Exception) {
        -1
    }

    /**
     * 关于页显示的版本号：安装包的 versionName，与 `app/pubspec.yaml` 的 version 一致
     * （发布时由 CI 按 tag 改写）。读不到就返回 null，界面自行隐藏该行。
     */
    private fun appVersion(): String? = try {
        val info = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            packageManager.getPackageInfo(packageName, PackageManager.PackageInfoFlags.of(0))
        } else {
            @Suppress("DEPRECATION")
            packageManager.getPackageInfo(packageName, 0)
        }
        info.versionName
    } catch (_: Exception) {
        null
    }

    /**
     * 用户可见的设备名。各厂商"设备名称"改名写入的位置不同，按品牌选优先级：
     *  小米/红米（HyperOS/MIUI）：改名写 persist.sys.device_name（并同步蓝牙名），
     *    Global device_name 停留在营销默认名（实测不随改名更新，不可信）；
     *  原生/三星等：About 手机改名写 Global device_name，蓝牙改名写 Secure bluetooth_name。
     *  最终回退：营销名（ro.product.marketname，如 "Xiaomi 14"），再退 厂商+型号。
     */
    private fun deviceName(): String {
        val xiaomi = Build.MANUFACTURER.contains("xiaomi", true) ||
                Build.MANUFACTURER.contains("redmi", true)
        if (xiaomi) {
            sysProp("persist.sys.device_name")?.let { return it }
            setting("secure", "bluetooth_name")?.let { return it }
            setting("global", "device_name")?.let { return it }
        } else {
            setting("global", "device_name")?.let { return it }
            setting("secure", "bluetooth_name")?.let { return it }
        }
        return prettyBuildName()
    }

    private fun setting(namespace: String, key: String): String? = try {
        val v = if (namespace == "secure") {
            Settings.Secure.getString(contentResolver, key)
        } else {
            Settings.Global.getString(contentResolver, key)
        }
        v?.trim()?.takeIf { it.isNotEmpty() }
    } catch (_: Exception) {
        null
    }

    // SystemProperties 是 @hide API，应用进程走 getprop 子进程读取（失败返回 null）
    private fun sysProp(name: String): String? = try {
        val p = ProcessBuilder("getprop", name).start()
        val v = p.inputStream.bufferedReader().readText().trim()
        p.waitFor()
        v.takeIf { it.isNotEmpty() }
    } catch (_: Exception) {
        null
    }

    private fun prettyBuildName(): String {
        sysProp("ro.product.marketname")?.let { return it }
        val maker = Build.MANUFACTURER.replaceFirstChar { it.uppercase() }
        val model = Build.MODEL
        return if (model.startsWith(maker, ignoreCase = true)) model else "$maker $model"
    }

    /**
     * 硬件指纹：Android ID（Settings.Secure）。
     * 同签名 APP 重装不变、系统升级不变、无需特殊权限。
     */
    private fun hardwareId(): String? = try {
        Settings.Secure.getString(contentResolver, Settings.Secure.ANDROID_ID)
            ?.trim()
            ?.takeIf { it.isNotEmpty() }
    } catch (_: Exception) {
        null
    }
}
