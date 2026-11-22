package dev.rdesk.rdesk

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings
import android.util.Log
import java.util.Locale

/**
 * "Start on its own": when the phone starts, and when it is unlocked (locking ends screen
 * sharing), rdesk asks for the screen again and starts the host, without the app's screen.
 *
 * Android still asks "Start recording or casting?" each time, unless the owner allowed rdesk
 * once over USB (`adb shell appops set dev.rdesk.rdesk PROJECT_MEDIA allow`). And Android lets
 * an app open that request from the background only while its accessibility service is on
 * (touch control); otherwise a notification offers it with a tap.
 */
object AutoStart {
    private const val PREFS = "rdesk"
    private const val KEY = "start_on_its_own"
    private const val CHANNEL = "rdesk-start"
    private const val NOTIFICATION_ID = 7430
    private const val TAG = "rdesk-autostart"

    fun enabled(context: Context): Boolean =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean(KEY, false)

    fun set(context: Context, on: Boolean) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean(KEY, on).apply()
    }

    /** Share the screen and start the host, unless it is on already. */
    fun begin(context: Context) {
        if (ScreenCaptureService.running) return
        val open = Intent(context, CaptureActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            context.startActivity(open)
        } catch (e: Exception) {
            Log.i(TAG, "cannot open the screen request from here; offering it in a notification", e)
        }
        // Android may silently refuse the start from the background: the notification stays as
        // the way to do it with a tap (it goes away once sharing starts).
        offer(context, open)
    }

    fun withdraw(context: Context) {
        context.getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
    }

    private fun offer(context: Context, open: Intent) {
        val pt = Locale.getDefault().language == "pt"
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, if (pt) "Ligar sozinho" else "Start on its own", NotificationManager.IMPORTANCE_DEFAULT),
        )
        val tap = PendingIntent.getActivity(context, 0, open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val n = Notification.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_share)
            .setContentTitle(if (pt) "rdesk: ligar o acesso a este celular" else "rdesk: turn access to this phone on")
            .setContentText(if (pt) "Toque para compartilhar a tela" else "Tap to share the screen")
            .setContentIntent(tap)
            .setGroup("rdesk-start")
            .setAutoCancel(true)
            .build()
        nm.notify(NOTIFICATION_ID, n)
    }

    /** The phone's name, as the app gives it to the host. */
    fun deviceName(context: Context): String =
        if (Build.VERSION.SDK_INT >= 25) {
            Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
        } else {
            null
        } ?: "${Build.MANUFACTURER} ${Build.MODEL}"
}
