//! Redis GPUI — a personal, privacy-first, write-safe Redis GUI.
//!
//! Phase 2: first vertical slice — connect → tab → browse keys → view
//! value, read-only. See PLAN.md section 6 (phase table).
//!
//! # The tokio ↔ GPUI integration (the known hard part; documented)
//!
//! redis-rs 1.8.1's `MultiplexedConnection` is runtime-tied in two ways
//! (both verified against the vendored source, see
//! `.omc/notepads/phase2-app/decisions.md`):
//!
//! 1. `Client::get_multiplexed_async_connection()` ends by spawning the
//!    pipeline **driver task** with `tokio::spawn` (`client.rs:612` ->
//!    `Runtime::spawn` -> `tokio::spawn`). `tokio::spawn` **panics**
//!    when called outside a tokio runtime context, so connection
//!    establishment MUST happen inside a tokio runtime.
//! 2. Its response-timeout path wraps requests in
//!    `tokio::time::timeout` (via `Runtime::locate()`), which requires
//!    an active tokio timer context — only guaranteed while polled from
//!    a tokio runtime thread.
//!
//! GPUI (pinned rev b5cd310) runs its own async executor (smol-based)
//! that is NOT a tokio runtime, so driving redis-rs futures directly on
//! GPUI's background executor would panic (1) and risk (2).
//!
//! **Chosen integration:** one global multi-thread tokio runtime,
//! created in `main()` before `application().run(...)` and leaked
//! (lives for the process lifetime; there is nothing meaningful to
//! shut down before exit). Every redis-io call (connect, reconnect,
//! scan batch, issue) runs inside `Handle::spawn(...)` on that runtime.
//! The returned `JoinHandle` is awaited *from GPUI's* foreground
//! executor (`cx.spawn`), so results are applied to GPUI entities on
//! the main thread. I/O never blocks the UI thread; the UI never
//! touches tokio directly. GPUI's own background executor is used only
//! for CPU work (rendering previews), not for redis futures.

mod console;
mod console_panel;
mod focus_ring;
mod modal;
mod palette_ext;
mod reply_to_raw;
mod saved_connections;
mod security;
mod session;
mod settings;
mod text_input;
mod ui_primitives;
mod value_editor;
mod workspace;

use gpui::{prelude::*, px, size, App, Bounds, Global, WindowBounds, WindowOptions};
use gpui_platform::application;
use std::sync::OnceLock;
use tokio::runtime::Runtime;

use crate::workspace::Workspace;

/// The one process-global tokio runtime that drives all redis-io I/O.
///
/// See the [module docs](self) for why redis-rs requires it.
pub struct TokioRuntime(pub tokio::runtime::Handle);

impl Global for TokioRuntime {}

/// Set the process-global tokio runtime (called once from `main`).
///
/// Leaks the runtime on purpose: it must outlive every connection, and
/// orderly shutdown buys nothing at process exit.
pub fn install_tokio_runtime() -> &'static tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let rt = Runtime::new().expect("failed to build the tokio runtime for redis I/O");
        // A multi-thread runtime keeps driver tasks off the main thread;
        // the Box::leak keeps it alive for the process lifetime.
        let handle = Box::leak(Box::new(rt)).handle().clone();
        handle
    })
}

fn main() {
    let tokio_handle = install_tokio_runtime();
    application().run(move |cx: &mut App| {
        cx.set_global(TokioRuntime(tokio_handle.clone()));
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(Workspace::new),
        )
        .unwrap();
        cx.activate(true);
    });
}
