package dev.rdesk.rdesk

import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Bundle
import io.flutter.embedding.android.FlutterFragmentActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

// A FragmentActivity: the device key's fingerprint prompt (BiometricPrompt) needs one.
class MainActivity : FlutterFragmentActivity() {
    private var multicast: WifiManager.MulticastLock? = null
    private val screen = ScreenShare(this)

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "dev.rdesk/device_key")
            .setMethodCallHandler(DeviceKey(this))
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "dev.rdesk/screen")
            .setMethodCallHandler(screen)
    }

    @Deprecated("Flutter plugins still deliver their results here")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (!screen.onActivityResult(requestCode, resultCode, data)) {
            @Suppress("DEPRECATION")
            super.onActivityResult(requestCode, resultCode, data)
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (!screen.onPermissionsResult(requestCode)) {
            super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Finding computers on the Wi-Fi (mDNS) needs multicast, which Android filters out
        // unless an app holds this lock.
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicast = wifi.createMulticastLock("rdesk-mdns").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    override fun onDestroy() {
        multicast?.release()
        super.onDestroy()
    }
}
