//! The look of the app and of the session window, and the pieces every screen is built
//! from. Screens use these instead of styling egui widgets by hand, so they all look and
//! behave the same.
//!
//! # Layout rules (no clipped or overflowing text)
//!
//! * Text widgets size to their text: no `add_sized` / exact widths on buttons, badges or
//!   chips (a minimum size is fine). Portuguese runs 20–30 % longer than English.
//! * A row with controls on the right uses [`row`]: the right side is laid out first and
//!   the left side gets what is left, with [`truncate`] (full text on hover) for names.
//! * Sentences wrap ([`paragraph`], [`caption`]); widths are a maximum, never a fixed size.
//! * Dialogs come from [`dialog`]: at most the window's width, scrolling when tall.
//! * Several actions in a line use `horizontal_wrapped`, so they go to the next line
//!   instead of past the edge.

pub mod icons;
mod theme;
mod toasts;
mod widgets;

pub use theme::*;
pub use toasts::*;
pub use widgets::*;
