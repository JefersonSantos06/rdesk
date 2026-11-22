package dev.rdesk.rdesk

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.graphics.drawable.Icon
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log
import android.view.Display
import java.util.Locale
import kotlin.math.roundToInt

/**
 * Shares the phone's screen with the rdesk host (Android requires a foreground service of
 * type `mediaProjection` for it, with its notification). The whole display is mirrored into
 * an [ImageReader] (scaled to at most [MAX_SIDE]); new pictures go to Rust through
 * [ScreenBridge], at most [MAX_FPS] a second, and always the newest one. A turned screen gets a
 * new size.
 *
 * Android ends the projection when the screen locks or when the person stops it from the
 * system bar; then this service stops, and with it the host.
 */
class ScreenCaptureService : Service() {
    companion object {
        const val ACTION_START = "dev.rdesk.rdesk.screen.START"
        const val ACTION_STOP = "dev.rdesk.rdesk.screen.STOP"
        const val EXTRA_RESULT_CODE = "resultCode"
        const val EXTRA_DATA = "data"
        private const val CHANNEL = "rdesk-sharing"
        private const val NOTIFICATION_ID = 7420
        private const val MAX_FPS = 30
        private const val MAX_SIDE = 1920
        private const val TAG = "rdesk-screen"

        /** Sharing now. */
        @Volatile
        var running = false
            private set

        /** Told (on the main thread) whether sharing started: `true` once pictures can flow. */
        var onStarted: ((Boolean) -> Unit)? = null
    }

    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    private var width = 0
    private var height = 0
    private var dpi = 0

    /** The newest picture not handed to Rust yet (only touched on [handler]'s thread). */
    private var pending: Image? = null
    private var lastPush = 0L
    private var stoppedByPerson = false
    private val push = Runnable { pushPending() }

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
        override fun onDisplayChanged(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) resizeIfTurned()
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stoppedByPerson = true
                stopSelf()
            }
            ACTION_START -> if (projection == null) start(intent) else answer(true)
            else -> stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun start(intent: Intent) {
        // Android 14+: the service must be in the foreground before the projection is taken.
        startForegroundNow()
        val code = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        val data: Intent? = if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableExtra(EXTRA_DATA, Intent::class.java)
        } else {
            @Suppress("DEPRECATION") intent.getParcelableExtra(EXTRA_DATA)
        }
        val p = try {
            data?.let { getSystemService(MediaProjectionManager::class.java).getMediaProjection(code, it) }
        } catch (e: Exception) {
            Log.w(TAG, "no projection", e)
            null
        }
        if (p == null) {
            answer(false)
            stopSelf()
            return
        }
        projection = p
        val t = HandlerThread("rdesk-screen").also { it.start() }
        thread = t
        val h = Handler(t.looper)
        handler = h
        // Android 14+: a callback before the virtual display.
        p.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Handler(mainLooper).post { stopSelf() }
            }
        }, h)
        measure()
        val r = newReader(h)
        reader = r
        display = p.createVirtualDisplay(
            "rdesk", width, height, dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, r.surface, null, h,
        )
        getSystemService(DisplayManager::class.java).registerDisplayListener(displayListener, h)
        running = true
        ScreenBridge.started(width, height)
        answer(true)
    }

    private fun answer(ok: Boolean) {
        onStarted?.invoke(ok)
        onStarted = null
    }

    override fun onDestroy() {
        getSystemService(DisplayManager::class.java).unregisterDisplayListener(displayListener)
        display?.release()
        display = null
        projection?.stop()
        projection = null
        val r = reader
        reader = null
        handler?.post {
            pending?.close()
            pending = null
            r?.close()
        }
        thread?.quitSafely()
        if (running) {
            running = false
            ScreenBridge.stopped(stoppedByPerson)
        }
        answer(false)
        super.onDestroy()
    }

    /**
     * The size to share the display at now; `true` if it changed. At most [MAX_SIDE] on the
     * longer side and a multiple of 16 on both: what hardware H.264 encoders take everywhere
     * (no row padding to guess). The display is mirrored into it, scaled.
     */
    @Suppress("DEPRECATION")
    private fun measure(): Boolean {
        val m = DisplayMetrics()
        getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY).getRealMetrics(m)
        val scale = minOf(1.0, MAX_SIDE.toDouble() / maxOf(m.widthPixels, m.heightPixels))
        val w = maxOf(16, (m.widthPixels * scale / 16).roundToInt() * 16)
        val h = maxOf(16, (m.heightPixels * scale / 16).roundToInt() * 16)
        val changed = w != width || h != height
        width = w
        height = h
        dpi = (m.densityDpi * scale).roundToInt()
        return changed
    }

    // Three images: one waiting for Rust, one being drawn, one spare for acquireLatestImage.
    private fun newReader(h: Handler): ImageReader =
        ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 3).apply {
            setOnImageAvailableListener({ onImage(it) }, h)
        }

    private fun onImage(r: ImageReader) {
        if (r !== reader) return
        val img = try {
            r.acquireLatestImage()
        } catch (e: IllegalStateException) {
            null
        } ?: return
        pending?.close()
        pending = img
        val h = handler ?: return
        h.removeCallbacks(push)
        val wait = lastPush + 1000L / MAX_FPS - SystemClock.uptimeMillis()
        if (wait <= 0) pushPending() else h.postDelayed(push, wait)
    }

    private fun pushPending() {
        val img = pending ?: return
        pending = null
        lastPush = SystemClock.uptimeMillis()
        try {
            val plane = img.planes[0]
            ScreenBridge.frame(plane.buffer, img.width, img.height, plane.rowStride)
        } catch (e: Exception) {
            Log.w(TAG, "picture not passed on", e)
        } finally {
            img.close()
        }
    }

    /** On [handler]'s thread: a turned screen gets a display and reader of its new size. */
    private fun resizeIfTurned() {
        if (!measure()) return
        val h = handler ?: return
        val old = reader
        pending?.close()
        pending = null
        val r = newReader(h)
        reader = r
        display?.resize(width, height, dpi)
        display?.surface = r.surface
        old?.close()
        ScreenBridge.started(width, height)
    }

    private fun t(en: String, pt: String) = if (Locale.getDefault().language == "pt") pt else en

    private fun startForegroundNow() {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, t("Screen sharing", "Compartilhamento da tela"), NotificationManager.IMPORTANCE_LOW),
        )
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        val stop = PendingIntent.getService(
            this, 1, Intent(this, ScreenCaptureService::class.java).setAction(ACTION_STOP), flags,
        )
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP), flags,
        )
        val n = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_share)
            .setContentTitle(t("Sharing this phone's screen", "Compartilhando a tela deste celular"))
            .setContentText(t("Whoever connects with rdesk sees it", "Quem se conectar pelo rdesk vê a tela"))
            .setOngoing(true)
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(null as Icon?, t("Stop", "Parar"), stop).build())
            .build()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIFICATION_ID, n)
        }
    }
}
