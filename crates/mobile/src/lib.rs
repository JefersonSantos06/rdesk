//! The phone app's Rust side. Everything under [`api`] is what the Flutter app calls
//! (`flutter_rust_bridge_codegen generate` turns it into Dart); the work is done by
//! `rdesk-core` and `rdesk-common`, the same code as the desktop app.

pub mod api;
mod frb_generated;
mod runtime;
