package dev.rdesk.rdesk

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import io.flutter.embedding.android.FlutterFragmentActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

// A FragmentActivity: the device key's fingerprint prompt (BiometricPrompt) needs one.
class MainActivity : FlutterFragmentActivity() {
    private var multicast: WifiManager.MulticastLock? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "dev.rdesk/device_key")
            .setMethodCallHandler(DeviceKey(this))
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
