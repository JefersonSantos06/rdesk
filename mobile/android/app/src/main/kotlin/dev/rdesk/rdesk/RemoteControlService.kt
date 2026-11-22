package dev.rdesk.rdesk

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.accessibilityservice.GestureDescription.StrokeDescription
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.graphics.Path
import android.hardware.display.DisplayManager
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log
import android.view.Display
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.view.accessibility.AccessibilityNodeInfo.AccessibilityAction
import kotlin.concurrent.thread

/**
 * Touches and typing from whoever is connected with rdesk (see `crates/mobile/src/touch.rs`
 * for the commands). Android lets only an accessibility service do this, and only after the
 * person turns it on in the system's settings.
 *
 * A held mouse button is a held finger: the stroke starts at `down` and goes on, piece by
 * piece, with [StrokeDescription.continueStroke] until `up`, so long presses and drags happen
 * as they are made. Commands run one at a time, in order, on the main thread: the next waits
 * for the gesture before it (a new gesture would cancel one in progress).
 */
class RemoteControlService : AccessibilityService() {
    companion object {
        private const val TAG = "rdesk-touch"

        /** Wheel notches per screen height. */
        private const val NOTCHES_PER_SCREEN = 12
    }

    private sealed interface Cmd
    private data class Down(val x: Float, val y: Float) : Cmd
    private data class Move(val x: Float, val y: Float) : Cmd
    private data class Up(val holdMs: Long) : Cmd
    private data class Scroll(val x: Float, val y: Float, val dx: Int, val dy: Int) : Cmd
    private data class Action(val line: String) : Cmd

    private val main = Handler(Looper.getMainLooper())

    @Volatile
    private var reading = false

    // Main thread only.
    private val queue = ArrayDeque<Cmd>()
    private var busy = false

    /** The finger now down (its stroke goes on), or `null`. */
    private var stroke: StrokeDescription? = null
    private var lastX = 0f
    private var lastY = 0f
    private var lastAt = 0L

    override fun onServiceConnected() {
        super.onServiceConnected()
        InputBridge.connected(true)
        reading = true
        thread(name = "rdesk-touch", isDaemon = true) {
            while (reading) {
                val line = InputBridge.next(250) ?: continue
                main.post { take(line) }
            }
        }
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    override fun onUnbind(intent: Intent?): Boolean {
        stopReading()
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        stopReading()
        super.onDestroy()
    }

    private fun stopReading() {
        if (!reading) return
        reading = false
        InputBridge.connected(false)
    }

    // ---- commands -------------------------------------------------------------------------

    /** From the shared picture's pixels to the display's. */
    @Suppress("DEPRECATION")
    private fun scale(): Pair<Float, Float> {
        val m = DisplayMetrics()
        getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY).getRealMetrics(m)
        val w = ScreenCaptureService.sharedWidth
        val h = ScreenCaptureService.sharedHeight
        return if (w > 0 && h > 0) m.widthPixels.toFloat() / w to m.heightPixels.toFloat() / h else 1f to 1f
    }

    private fun take(line: String) {
        val parts = line.split(' ')
        val num = { i: Int -> parts.getOrNull(i)?.toFloatOrNull() ?: 0f }
        val (sx, sy) = scale()
        val cmd = when (parts[0]) {
            "down" -> Down(num(1) * sx, num(2) * sy)
            "move" -> Move(num(1) * sx, num(2) * sy)
            "up" -> Up(1)
            "scroll" -> Scroll(num(1) * sx, num(2) * sy, num(3).toInt(), num(4).toInt())
            else -> Action(line)
        }
        queue.addLast(cmd)
        pump()
    }

    private fun pump() {
        while (!busy) {
            when (val c = queue.removeFirstOrNull() ?: return) {
                is Down -> {
                    lastX = c.x
                    lastY = c.y
                    dispatch(StrokeDescription(pointAt(c.x, c.y), 0, 1, true), keep = true)
                }
                is Move -> {
                    val s = stroke ?: continue
                    var to = c
                    while (queue.firstOrNull() is Move) to = queue.removeFirst() as Move
                    if (to.x == lastX && to.y == lastY) continue
                    val path = Path().apply {
                        moveTo(lastX, lastY)
                        lineTo(to.x, to.y)
                    }
                    lastX = to.x
                    lastY = to.y
                    val ms = (SystemClock.uptimeMillis() - lastAt).coerceIn(8, 100)
                    dispatch(s.continueStroke(path, 0, ms, true), keep = true)
                }
                is Up -> {
                    val s = stroke ?: continue
                    dispatch(s.continueStroke(pointAt(lastX, lastY), 0, c.holdMs, false), keep = false)
                }
                is Scroll -> {
                    if (stroke != null) continue
                    var dx = c.dx
                    var dy = c.dy
                    while (true) {
                        val n = queue.firstOrNull() as? Scroll ?: break
                        queue.removeFirst()
                        dx += n.dx
                        dy += n.dy
                    }
                    swipe(c.x, c.y, dx, dy)
                }
                is Action -> act(c.line)
            }
        }
    }

    /** A wheel turn: the finger slides the other way, then rests so the list does not fling. */
    @Suppress("DEPRECATION")
    private fun swipe(x: Float, y: Float, dx: Int, dy: Int) {
        val m = DisplayMetrics()
        getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY).getRealMetrics(m)
        val w = m.widthPixels.toFloat()
        val h = m.heightPixels.toFloat()
        val fromX = x.coerceIn(1f, w - 2)
        val fromY = y.coerceIn(1f, h - 2)
        val toX = (fromX - dx * w / NOTCHES_PER_SCREEN).coerceIn(1f, w - 2)
        val toY = (fromY + dy * h / NOTCHES_PER_SCREEN).coerceIn(1f, h - 2)
        if (toX == fromX && toY == fromY) return
        val path = Path().apply {
            moveTo(fromX, fromY)
            lineTo(toX, toY)
        }
        lastX = toX
        lastY = toY
        queue.addFirst(Up(60))
        dispatch(StrokeDescription(path, 0, 200, true), keep = true)
    }

    private fun pointAt(x: Float, y: Float) = Path().apply { moveTo(x, y) }

    private fun dispatch(s: StrokeDescription, keep: Boolean) {
        busy = true
        lastAt = SystemClock.uptimeMillis()
        val gesture = GestureDescription.Builder().addStroke(s).build()
        val sent = dispatchGesture(gesture, object : GestureResultCallback() {
            override fun onCompleted(gestureDescription: GestureDescription?) {
                stroke = if (keep) s else null
                busy = false
                pump()
            }

            override fun onCancelled(gestureDescription: GestureDescription?) {
                // Something else touched the screen (the person at the phone): start over.
                stroke = null
                busy = false
                while (queue.firstOrNull() is Move || queue.firstOrNull() is Up) queue.removeFirst()
                pump()
            }
        }, main)
        if (!sent) {
            Log.w(TAG, "gesture not dispatched")
            stroke = null
            busy = false
        }
    }

    // ---- actions and typing ---------------------------------------------------------------

    private fun act(line: String) {
        val word = line.substringBefore(' ')
        val rest = line.substringAfter(' ', "")
        try {
            when (word) {
                "back" -> performGlobalAction(GLOBAL_ACTION_BACK)
                "home" -> performGlobalAction(GLOBAL_ACTION_HOME)
                "recents" -> performGlobalAction(GLOBAL_ACTION_RECENTS)
                "screenshot" -> if (Build.VERSION.SDK_INT >= 28) performGlobalAction(GLOBAL_ACTION_TAKE_SCREENSHOT)
                "volume" -> volume(rest)
                "text" -> type(rest)
                "key" -> key(rest)
                else -> Log.w(TAG, "unknown command: $word")
            }
        } catch (e: Exception) {
            Log.w(TAG, "$word failed", e)
        }
    }

    private fun volume(how: String) {
        val audio = getSystemService(AudioManager::class.java)
        val dir = when (how) {
            "up" -> AudioManager.ADJUST_RAISE
            "down" -> AudioManager.ADJUST_LOWER
            else -> AudioManager.ADJUST_TOGGLE_MUTE
        }
        audio.adjustStreamVolume(AudioManager.STREAM_MUSIC, dir, AudioManager.FLAG_SHOW_UI)
    }

    /** The field being typed into, if any. */
    private fun field(): AccessibilityNodeInfo? =
        (rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) ?: findFocus(AccessibilityNodeInfo.FOCUS_INPUT))
            ?.takeIf { it.isEditable }

    /** The field's text (not its hint) and selection, in order. */
    private fun content(n: AccessibilityNodeInfo): Triple<String, Int, Int> {
        // The node is a snapshot: without this, a key right after another reads the text before
        // it (and the earlier key is lost).
        n.refresh()
        val text = if (Build.VERSION.SDK_INT >= 26 && n.isShowingHintText) "" else n.text?.toString() ?: ""
        var a = n.textSelectionStart
        var b = n.textSelectionEnd
        if (a < 0 || b < 0 || a > text.length || b > text.length) {
            a = text.length
            b = text.length
        }
        return Triple(text, minOf(a, b), maxOf(a, b))
    }

    private fun setText(n: AccessibilityNodeInfo, text: String, cursor: Int) {
        n.performAction(
            AccessibilityNodeInfo.ACTION_SET_TEXT,
            Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text) },
        )
        select(n, cursor, cursor)
    }

    private fun select(n: AccessibilityNodeInfo, a: Int, b: Int) {
        n.performAction(
            AccessibilityNodeInfo.ACTION_SET_SELECTION,
            Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, a)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, b)
            },
        )
    }

    private fun type(s: String) {
        val n = field() ?: return
        if (n.isPassword) {
            // Its text cannot be read back, so it cannot be edited around: paste instead.
            getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("rdesk", s))
            n.performAction(AccessibilityNodeInfo.ACTION_PASTE)
            return
        }
        val (text, a, b) = content(n)
        setText(n, text.substring(0, a) + s + text.substring(b), a + s.length)
    }

    private fun key(name: String) {
        val n = field() ?: return
        val (text, a, b) = content(n)
        when (name) {
            "backspace" -> when {
                a < b -> setText(n, text.removeRange(a, b), a)
                a > 0 -> {
                    val from = text.offsetByCodePoints(a, -1)
                    setText(n, text.removeRange(from, a), from)
                }
            }
            "delete" -> when {
                a < b -> setText(n, text.removeRange(a, b), a)
                b < text.length -> setText(n, text.removeRange(b, text.offsetByCodePoints(b, 1)), b)
            }
            "enter" -> if (Build.VERSION.SDK_INT >= 30) {
                n.performAction(AccessibilityAction.ACTION_IME_ENTER.id)
            } else if (n.isMultiLine) {
                type("\n")
            }
            "left" -> (if (a > 0) text.offsetByCodePoints(a, -1) else 0).let { select(n, it, it) }
            "right" -> (if (b < text.length) text.offsetByCodePoints(b, 1) else text.length).let { select(n, it, it) }
            "home" -> select(n, 0, 0)
            "end" -> select(n, text.length, text.length)
            "select_all" -> select(n, 0, text.length)
            "copy" -> n.performAction(AccessibilityNodeInfo.ACTION_COPY)
            "cut" -> n.performAction(AccessibilityNodeInfo.ACTION_CUT)
            "paste" -> n.performAction(AccessibilityNodeInfo.ACTION_PASTE)
        }
    }
}
