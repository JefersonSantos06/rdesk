package dev.rdesk.rdesk

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** The phone started (or rdesk was updated): with "start on its own" on, see [AutoStart]. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED ->
                if (AutoStart.enabled(context)) AutoStart.begin(context)
        }
    }
}
