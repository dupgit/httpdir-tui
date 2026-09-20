//! Everything that talks to the network lives here, in a background task.
//!
//! This isn't a ratatui concern, but it's the piece that makes a
//! *responsive* ratatui UI possible: `terminal.draw` and
//! `event::poll` in `main.rs` must keep running every ~100ms no matter
//! how slow the remote server is, or the whole terminal would appear
//! frozen while waiting for HTTP. So the UI thread never `.await`s an
//! HTTP request directly — it hands the work to `tokio::spawn` and goes
//! back to its loop immediately, then picks up the result later from a
//! channel (see `main.rs`'s `rx.try_recv()`).

use httpdirectory::httpdirectory::HttpDirectory;
use tokio::sync::mpsc::UnboundedSender;

/// Explicit request timeout. Without one, a stalled or malicious server
/// could hang the fetch task forever; the UI would just say "Loading..."
/// indefinitely. Adjust to taste, but always set *something*.
const TIMEOUT_S: u64 = 10;

/// What the background task should do next.
pub enum FetchRequest {
    /// First load of a given URL.
    Root(String),
    /// Descend from an already-fetched listing into one of its entries.
    /// Carrying the previous `HttpDirectory` (instead of just a URL string)
    /// lets `cd` reuse it directly and keeps the call site symmetrical with
    /// the crate's own API.
    Cd(HttpDirectory, String),
}

/// What comes back from the background task, sent over the channel to
/// be picked up by the main loop in `main.rs` and applied to `App` via
/// `App::apply_fetch_result`.
pub enum FetchMessage {
    Loaded(HttpDirectory),
    Failed(String),
}

/// Spawns the actual network call on a new tokio task and sends its
/// outcome back through `tx` once it completes. Returns immediately —
/// the caller never waits for the network here.
pub fn spawn_fetch(tx: UnboundedSender<FetchMessage>, request: FetchRequest) {
    tokio::spawn(async move {
        // Both branches are `async fn`s from the `httpdirectory` crate;
        // `.await` suspends this task (not the whole program) until the
        // HTTP request completes.
        let result = match request {
            FetchRequest::Root(url) => HttpDirectory::new(&url, Some(TIMEOUT_S)).await,
            FetchRequest::Cd(dir, target) => dir.cd(&target).await,
        };

        let message = match result {
            Ok(dir) => FetchMessage::Loaded(dir),
            Err(err) => FetchMessage::Failed(err.to_string()),
        };

        // `send` only fails if the receiving end (in `main.rs`) was
        // dropped, which happens when the UI already exited — nothing
        // useful to do about it at that point, so the error is ignored.
        let _ = tx.send(message);
    });
}
