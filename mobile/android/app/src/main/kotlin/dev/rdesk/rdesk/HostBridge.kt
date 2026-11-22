package dev.rdesk.rdesk

/**
 * What the host on the phone asks of the app while the screen is shared
 * (`crates/mobile/src/bridge.rs`): text for the clipboard, and questions for the person at
 * the phone. [ScreenCaptureService] takes them and answers.
 */
object HostBridge {
    init {
        System.loadLibrary("rdesk_mobile")
    }

    /** The next request, waiting up to `timeoutMs`; `null` if none came. */
    @JvmStatic external fun next(timeoutMs: Int): String?

    /** The person answered question `id`. */
    @JvmStatic external fun answer(id: Int, allowed: Boolean)

    /** Start the host without the app's screen (the screen is already shared); `true` if it did. */
    @JvmStatic external fun startHost(dataDir: String, deviceName: String, portuguese: Boolean): Boolean
}
