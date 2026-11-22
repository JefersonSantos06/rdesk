package dev.rdesk.rdesk

import android.Manifest
import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.graphics.drawable.Icon
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.Image
import android.media.ImageReader
import android.media.MediaCodec
import android.media.MediaFormat
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
import kotlin.concurrent.thread
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
 *
 * While it runs it also does what the host asks of the app ([HostBridge]): it puts text from
 * the other side on the clipboard, and asks the person at the phone (a notification with
 * Allow and Deny) before a connection starts.
 *
 * And while someone listens ([AudioBridge.wanted]) it captures what the phone plays (Android
 * 10+, with the microphone permission: apps may keep their sound out) and sends it as Opus.
 */
class ScreenCaptureService : Service() {
    companion object {
        const val ACTION_START = "dev.rdesk.rdesk.screen.START"
        const val ACTION_STOP = "dev.rdesk.rdesk.screen.STOP"
        const val ACTION_ANSWER = "dev.rdesk.rdesk.screen.ANSWER"
        const val EXTRA_ID = "id"
        const val EXTRA_ALLOW = "allow"
        private const val CHANNEL_ASK = "rdesk-ask"
        private const val ASK_NOTIFICATION_BASE = 7500
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

        /** The shared picture's size: touches from the other side come in its pixels. */
        @Volatile
        var sharedWidth = 0
            private set

        @Volatile
        var sharedHeight = 0
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

    @Volatile
    private var bridgeReading = false

    @Volatile
    private var audioReading = false

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
            ACTION_ANSWER -> {
                val id = intent.getIntExtra(EXTRA_ID, 0)
                HostBridge.answer(id, intent.getBooleanExtra(EXTRA_ALLOW, false))
                getSystemService(NotificationManager::class.java).cancel(ASK_NOTIFICATION_BASE + id)
            }
            else -> if (projection == null) stopSelf()
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
        sharedWidth = width
        sharedHeight = height
        ScreenBridge.started(width, height)
        if (Build.VERSION.SDK_INT >= 29 &&
            checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        ) {
            audioReading = true
            AudioBridge.available(true)
            thread(name = "rdesk-audio", isDaemon = true) { audioLoop(p) }
        }
        bridgeReading = true
        thread(name = "rdesk-bridge", isDaemon = true) {
            while (bridgeReading) {
                val req = HostBridge.next(250) ?: continue
                Handler(mainLooper).post { onRequest(req) }
            }
        }
        answer(true)
    }

    /** Capture while someone listens; idle otherwise. */
    private fun audioLoop(p: MediaProjection) {
        while (audioReading) {
            if (!AudioBridge.wanted()) {
                Thread.sleep(200)
                continue
            }
            try {
                captureWhileWanted(p)
            } catch (e: Exception) {
                Log.w(TAG, "sound", e)
                Thread.sleep(1000)
            }
        }
    }

    /** What the phone plays, 20 ms at a time, as Opus packets. */
    @SuppressLint("MissingPermission", "NewApi")
    private fun captureWhileWanted(p: MediaProjection) {
        val config = AudioPlaybackCaptureConfiguration.Builder(p)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        val frameBytes = 48000 / 50 * 2 * 2
        val rec = AudioRecord.Builder()
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(48000)
                    .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                    .build(),
            )
            .setBufferSizeInBytes(frameBytes * 8)
            .setAudioPlaybackCaptureConfig(config)
            .build()
        val enc = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
        val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, 48000, 2).apply {
            setInteger(MediaFormat.KEY_BIT_RATE, 96000)
        }
        enc.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        enc.start()
        rec.startRecording()
        val pcm = ByteArray(frameBytes)
        val info = MediaCodec.BufferInfo()
        var pts = 0L
        try {
            while (audioReading && AudioBridge.wanted()) {
                var got = 0
                while (got < frameBytes) {
                    val n = rec.read(pcm, got, frameBytes - got)
                    if (n <= 0) break
                    got += n
                }
                if (got < frameBytes) continue
                val i = enc.dequeueInputBuffer(10_000)
                if (i >= 0) {
                    enc.getInputBuffer(i)?.apply {
                        clear()
                        put(pcm, 0, got)
                    }
                    enc.queueInputBuffer(i, 0, got, pts, 0)
                    pts += 20_000
                }
                while (true) {
                    val o = enc.dequeueOutputBuffer(info, 0)
                    if (o < 0) break
                    if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0 && info.size > 0) {
                        val out = ByteArray(info.size)
                        enc.getOutputBuffer(o)?.apply {
                            position(info.offset)
                            get(out)
                        }
                        AudioBridge.packet(out, out.size)
                    }
                    enc.releaseOutputBuffer(o, false)
                }
            }
        } finally {
            rec.stop()
            rec.release()
            enc.stop()
            enc.release()
        }
    }

    /** What the host asks (see [HostBridge]): on the main thread. */
    private fun onRequest(req: String) {
        val kind = req.substringBefore('\n')
        val rest = req.substringAfter('\n', "")
        val nm = getSystemService(NotificationManager::class.java)
        when (kind) {
            "clip" -> getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("rdesk", rest))
            "ask" -> {
                val id = rest.substringBefore('\n').toIntOrNull() ?: return
                ask(id, rest.substringAfter('\n', ""))
            }
            "unask" -> rest.toIntOrNull()?.let { nm.cancel(ASK_NOTIFICATION_BASE + it) }
        }
    }

    /** Ask the person at the phone: a notification that pops up, with Allow and Deny. */
    private fun ask(id: Int, question: String) {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_ASK, t("Access requests", "Pedidos de acesso"), NotificationManager.IMPORTANCE_HIGH),
        )
        val reply = { allow: Boolean ->
            PendingIntent.getService(
                this,
                id * 2 + if (allow) 1 else 0,
                Intent(this, ScreenCaptureService::class.java)
                    .setAction(ACTION_ANSWER)
                    .putExtra(EXTRA_ID, id)
                    .putExtra(EXTRA_ALLOW, allow),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
        }
        val n = Notification.Builder(this, CHANNEL_ASK)
            .setSmallIcon(R.drawable.ic_stat_share)
            .setContentTitle(t("rdesk: allow access?", "rdesk: permitir acesso?"))
            .setContentText(question)
            .setStyle(Notification.BigTextStyle().bigText(question))
            .setCategory(Notification.CATEGORY_CALL)
            // A group of its own: grouped with the sharing notice, Android folds it away and
            // its buttons do not show.
            .setGroup("rdesk-ask-$id")
            // Shown in full while the screen is shared (Android hides private notifications'
            // content then, buttons included); it holds nothing secret.
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setAutoCancel(true)
            // Swiped away: a no.
            .setDeleteIntent(reply(false))
            .addAction(Notification.Action.Builder(null as Icon?, t("Deny", "Recusar"), reply(false)).build())
            .addAction(Notification.Action.Builder(null as Icon?, t("Allow", "Permitir"), reply(true)).build())
            .build()
        nm.notify(ASK_NOTIFICATION_BASE + id, n)
    }

    private fun answer(ok: Boolean) {
        onStarted?.invoke(ok)
        onStarted = null
    }

    override fun onDestroy() {
        bridgeReading = false
        if (audioReading) {
            audioReading = false
            AudioBridge.available(false)
        }
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
        sharedWidth = width
        sharedHeight = height
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
            .setGroup("rdesk-sharing")
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
