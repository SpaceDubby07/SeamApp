//! macOS platform implementation: clipboard access via `NSPasteboard`
//! through the `objc2`/`objc2-app-kit` crates.

mod clipboard;

pub use clipboard::Clipboard;
