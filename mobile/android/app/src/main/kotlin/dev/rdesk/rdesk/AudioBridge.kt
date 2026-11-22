package dev.rdesk.rdesk

/**
 * The Rust side of the phone's sound (`crates/mobile/src/sound.rs`): Opus packets of what the
 * phone plays, for whoever is listening.
 */
object AudioBridge {
    init {
        System.loadLibrary("rdesk_mobile")
    }

    /** The phone's sound can be captured now (sharing, Android 10+, the permission given). */
    @JvmStatic external fun available(on: Boolean)

    /** Someone is listening: capture and send while this is true. */
    @JvmStatic external fun wanted(): Boolean

    /** One Opus packet (20 ms, 48 kHz, stereo): the first `len` bytes of `data`. */
    @JvmStatic external fun packet(data: ByteArray, len: Int)
}
