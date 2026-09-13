//! Windows platform implementation: clipboard access via the `windows`
//! crate's clipboard APIs.

mod clipboard;

pub use clipboard::Clipboard;
