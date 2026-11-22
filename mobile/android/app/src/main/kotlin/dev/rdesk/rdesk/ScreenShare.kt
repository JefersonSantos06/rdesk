package dev.rdesk.rdesk

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.Settings
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel

/**
 * Screen sharing for the app (channel `dev.rdesk/screen`):
 * - `start` asks to show notifications (Android 13+, for the "sharing" notice), then Android's
 *   screen capture consent (the whole screen, Android 14+), and starts [ScreenCaptureService];
 *   answers `true` once pictures can flow, `false` if the person said no.
 * - `stop` ends sharing; `isSharing` tells whether it is on.
 * - `touchEnabled` tells whether touch control ([RemoteControlService]) is on in the system's
 *   accessibility settings; `openTouchSettings` opens them (at rdesk's entry where the system
 *   allows), `openAppInfo` opens the app's page (where Android 13+ hides "Allow restricted
 *   settings" for apps installed outside a store).
 * - `allFiles` tells whether the other side may browse all of the phone's files;
 *   `openAllFiles` asks for it (Android 11+: the system's "All files access" page; before:
 *   the storage permission).
 */
class ScreenShare(private val activity: Activity) : MethodChannel.MethodCallHandler {
    companion object {
        const val REQUEST_CAPTURE = 0x5244
        const val REQUEST_NOTIFICATIONS = 0x5245
        const val REQUEST_STORAGE = 0x5246
    }

    private var waiting: MethodChannel.Result? = null

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        when (call.method) {
            "start" -> when {
                ScreenCaptureService.running -> result.success(true)
                waiting != null -> result.error("busy", "already asking", null)
                else -> {
                    waiting = result
                    askNotifications()
                }
            }
            "stop" -> {
                activity.stopService(Intent(activity, ScreenCaptureService::class.java))
                result.success(null)
            }
            "isSharing" -> result.success(ScreenCaptureService.running)
            "touchEnabled" -> result.success(touchEnabled())
            "openTouchSettings" -> {
                openTouchSettings()
                result.success(null)
            }
            "allFiles" -> result.success(allFiles())
            "autoStart" -> result.success(AutoStart.enabled(activity))
            "setAutoStart" -> {
                AutoStart.set(activity, call.arguments as? Boolean ?: false)
                result.success(null)
            }
            "openAllFiles" -> {
                openAllFiles()
                result.success(null)
            }
            "openAppInfo" -> {
                activity.startActivity(
                    Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", activity.packageName, null)),
                )
                result.success(null)
            }
            else -> result.notImplemented()
        }
    }

    private fun allFiles(): Boolean = if (Build.VERSION.SDK_INT >= 30) {
        Environment.isExternalStorageManager()
    } else {
        activity.checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED
    }

    private fun openAllFiles() {
        if (Build.VERSION.SDK_INT >= 30) {
            try {
                activity.startActivity(
                    Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, Uri.fromParts("package", activity.packageName, null)),
                )
            } catch (e: ActivityNotFoundException) {
                activity.startActivity(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))
            }
        } else {
            activity.requestPermissions(
                arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE, Manifest.permission.WRITE_EXTERNAL_STORAGE),
                REQUEST_STORAGE,
            )
        }
    }

    private fun touchService() = ComponentName(activity, RemoteControlService::class.java)

    private fun touchEnabled(): Boolean {
        val on = Settings.Secure.getString(activity.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: ""
        val me = touchService()
        return on.split(':').any { ComponentName.unflattenFromString(it) == me }
    }

    private fun openTouchSettings() {
        // Android 11+ can open rdesk's own entry; elsewhere, the accessibility list.
        if (Build.VERSION.SDK_INT >= 30) {
            try {
                activity.startActivity(
                    Intent("android.settings.ACCESSIBILITY_DETAILS_SETTINGS")
                        .putExtra(Intent.EXTRA_COMPONENT_NAME, touchService().flattenToString()),
                )
                return
            } catch (e: ActivityNotFoundException) {
                // Not on this phone: the list below.
            } catch (e: SecurityException) {
                // Not allowed here: the list below.
            }
        }
        activity.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
    }

    /** Notifications (the sharing notice, the access questions) and the microphone (for the
     *  phone's sound: Android counts capturing what apps play as recording), then the screen.
     *  Both are optional: sharing works without them. */
    private fun askNotifications() {
        val missing = buildList {
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
            if (Build.VERSION.SDK_INT >= 29) add(Manifest.permission.RECORD_AUDIO)
        }.filter { activity.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (missing.isNotEmpty()) {
            activity.requestPermissions(missing.toTypedArray(), REQUEST_NOTIFICATIONS)
        } else {
            askCapture()
        }
    }

    /** From the activity; `true` if it was ours. Sharing works without the notice too. */
    fun onPermissionsResult(requestCode: Int): Boolean {
        if (requestCode != REQUEST_NOTIFICATIONS || waiting == null) return false
        askCapture()
        return true
    }

    private fun askCapture() {
        val mpm = activity.getSystemService(MediaProjectionManager::class.java)
        // Android 14+ would also offer a single app; rdesk shares the whole screen.
        val intent = if (Build.VERSION.SDK_INT >= 34) {
            mpm.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            mpm.createScreenCaptureIntent()
        }
        activity.startActivityForResult(intent, REQUEST_CAPTURE)
    }

    /** From the activity; `true` if it was ours. */
    fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?): Boolean {
        if (requestCode != REQUEST_CAPTURE) return false
        val result = waiting ?: return true
        if (resultCode != Activity.RESULT_OK || data == null) {
            waiting = null
            result.success(false)
            return true
        }
        ScreenCaptureService.onStarted = { ok ->
            waiting = null
            result.success(ok)
        }
        activity.startForegroundService(
            Intent(activity, ScreenCaptureService::class.java)
                .setAction(ScreenCaptureService.ACTION_START)
                .putExtra(ScreenCaptureService.EXTRA_RESULT_CODE, resultCode)
                .putExtra(ScreenCaptureService.EXTRA_DATA, data),
        )
        return true
    }
}
