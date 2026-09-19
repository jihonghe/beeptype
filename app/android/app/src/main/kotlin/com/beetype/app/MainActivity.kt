package com.beetype.app

import android.os.Build
import android.provider.Settings
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "beetype/device")
            .setMethodCallHandler { call, result ->
                if (call.method == "getDeviceName") {
                    result.success(deviceName())
                } else {
                    result.notImplemented()
                }
            }
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
}
