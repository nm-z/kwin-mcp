pub mod browser_cookies;
pub mod clipboard;
pub mod dsc;
pub mod links;
pub mod page_storage;
pub mod remote;
pub mod viewer_stream;

/// A session's virtual display when neither the server's `--width`/`--height`
/// nor `session_start`'s `width`/`height` give a size. A stdio server and a
/// `--listen` server have the same default (#984), and `kwin-viewer` started
/// by hand without a size assumes it.
pub const VIRTUAL_SCREEN_WIDTH: u32 = 1920;
pub const VIRTUAL_SCREEN_HEIGHT: u32 = 1080;
