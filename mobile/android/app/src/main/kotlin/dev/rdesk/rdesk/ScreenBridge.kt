package dev.rdesk.rdesk

import java.nio.ByteBuffer

/**
 * The Rust side of screen sharing (`crates/mobile/src/screen.rs`): the host there sends
 * what these calls hand it. The library is the one Flutter already loaded, so both see the
 * same state.
 */
object ScreenBridge {
    init {
        System.loadLibrary("rdesk_mobile")
    }

    /** Sharing started, or the screen turned: its size in pixels. */
    @JvmStatic external fun started(width: Int, height: Int)

    /** A new picture: RGBA rows `rowStride` bytes apart in a direct buffer. */
    @JvmStatic external fun frame(buffer: ByteBuffer, width: Int, height: Int, rowStride: Int)

    /** Sharing ended; `byPerson`: from the notification's Stop button. The host stops too. */
    @JvmStatic external fun stopped(byPerson: Boolean)
}
