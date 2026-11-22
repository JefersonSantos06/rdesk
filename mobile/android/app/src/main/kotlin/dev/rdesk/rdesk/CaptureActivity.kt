package dev.rdesk.rdesk

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import java.util.Locale

/**
 * "Start on its own" ([AutoStart]): an invisible screen that only asks Android for the whole
 * screen, starts sharing it, and starts the host; then closes.
 */
class CaptureActivity : Activity() {
    companion object {
        private const val REQUEST_CAPTURE = 0x5247
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        AutoStart.withdraw(this)
        if (ScreenCaptureService.running) {
            finish()
            return
        }
        val mpm = getSystemService(MediaProjectionManager::class.java)
        val intent = if (Build.VERSION.SDK_INT >= 34) {
            mpm.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            mpm.createScreenCaptureIntent()
        }
        @Suppress("DEPRECATION")
        startActivityForResult(intent, REQUEST_CAPTURE)
    }

    @Deprecated("The plain Activity result API, on purpose: this screen has nothing else")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_CAPTURE || resultCode != RESULT_OK || data == null) {
            finish()
            return
        }
        val app = applicationContext
        ScreenCaptureService.onStarted = { ok ->
            if (ok) {
                HostBridge.startHost(app.filesDir.absolutePath, AutoStart.deviceName(app), Locale.getDefault().language == "pt")
            }
        }
        startForegroundService(
            Intent(this, ScreenCaptureService::class.java)
                .setAction(ScreenCaptureService.ACTION_START)
                .putExtra(ScreenCaptureService.EXTRA_RESULT_CODE, resultCode)
                .putExtra(ScreenCaptureService.EXTRA_DATA, data),
        )
        finish()
    }
}
