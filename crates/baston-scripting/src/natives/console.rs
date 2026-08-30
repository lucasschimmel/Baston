//! The server console buffer behind `GetConsoleBuffer` / `print`.
//!
//! Process-wide, and engine-neutral for the same reason: `GetConsoleBuffer` is
//! documented to return what the *server* printed, not what the calling
//! resource printed — so a JS resource must see a Lua resource's output and
//! vice versa.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// Lines retained for `GetConsoleBuffer`. Bounded: the buffer is a debugging
/// convenience, and a chatty resource must not turn it into a memory leak.
const CONSOLE_BUFFER_LINES: usize = 512;

/// Recent console output, oldest first.
fn console_buffer() -> &'static Mutex<VecDeque<String>> {
    static BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Record one line of resource output and echo it to stdout.
pub(crate) fn log(resource: &str, msg: &str) {
    tracing::info!(target: "script", resource, "{msg}");
    {
        let mut buffer = console_buffer().lock().unwrap_or_else(|e| e.into_inner());
        if buffer.len() == CONSOLE_BUFFER_LINES {
            buffer.pop_front();
        }
        buffer.push_back(format!("[{resource}] {msg}"));
    }
    println!("{msg}");
}

/// The retained console output as one blob, which is what the native returns.
pub(crate) fn console_buffer_text() -> String {
    let buffer = console_buffer().lock().unwrap_or_else(|e| e.into_inner());
    buffer.iter().cloned().collect::<Vec<_>>().join("\n")
}
