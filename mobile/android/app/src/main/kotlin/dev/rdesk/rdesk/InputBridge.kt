package dev.rdesk.rdesk

/**
 * The Rust side of remote control (`crates/mobile/src/touch.rs`): what the other side does
 * with its mouse and keyboard, as commands for [RemoteControlService].
 */
object InputBridge {
    init {
        System.loadLibrary("rdesk_mobile")
    }

    /** The service started (`true`) or stopped taking commands. */
    @JvmStatic external fun connected(on: Boolean)

    /** The next command, waiting up to `timeoutMs`; `null` if none came. */
    @JvmStatic external fun next(timeoutMs: Int): String?
}
