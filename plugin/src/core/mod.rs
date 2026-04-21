//! Core MIDI command logic — ported from `src/core/sender_core.{h,cpp}` in the
//! original C++ project (see ../../../ccomidi).
//!
//! This module is deliberately *framework-agnostic*:
//!   - no dependency on `nih-plug`,
//!   - no knowledge of audio threads or plugin hosts,
//!   - emission happens through the [`EventSink`] trait so the plugin, tests,
//!     or any other caller can plug in whatever destination they want.
//!
//! The public entry point is [`SenderCore`]. Read its top-of-struct doc for
//! the big picture.
//!
//! # Rust notes for newcomers
//!
//! - `mod foo;` declares a submodule whose code lives in `foo.rs` (or
//!   `foo/mod.rs`). Items in a submodule are private by default; `pub use`
//!   at the parent re-exports them so callers can write `core::SenderCore`
//!   instead of `core::sender::SenderCore`.
//! - Files in the same module can see each other's private items via
//!   `super::` or `crate::`. This mirrors C++'s "same translation unit"
//!   rules but is enforced by the compiler.

mod command;
mod encode;
mod sender;

// Re-export the parts callers should be able to reach without knowing the
// internal file split. Everything else stays private.
pub use command::{CommandType, FIXED_ROW_COUNT, MAX_FIELDS, MAX_MESSAGES_PER_ROW, MAX_ROWS};
pub use encode::{encode_row, CcMessage, EncodedCommand};
pub use sender::{EventSink, RowState, SenderCore};
