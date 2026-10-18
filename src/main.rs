//! Entry point: terminal setup/teardown and the main event loop.
//!
//! ratatui does not own the terminal by itself — it draws into whatever
//! backend you give it (here, crossterm). Two things crossterm does that
//! ratatui relies on:
//!   - "raw mode": the terminal stops line-buffering and echoing input,
//!     so we get individual key presses instead of whole lines typed
//!     after Enter.
//!   - "alternate screen": a second, blank terminal buffer that the
//!     program draws into, leaving the user's normal shell scrollback
//!     untouched. Leaving it restores exactly what was on screen before
//!     the program started.
//!
//! Both must be explicitly entered on startup and left on exit — ratatui
//! does not do this automatically, which is why `main` wraps `run(...)`
//! and always restores the terminal afterwards, even on error.

mod app;
mod cli;
mod download;
mod fetch;
mod ui;

use app::{App, SortKey};
use cli::Cli;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use download::{DownloadMessage, Downloader};
use fetch::{FetchMessage, FetchRequest, spawn_fetch};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

/// Number of lines a `PageUp` or a `PageDown` key moves in the file list
const PAGE_UP_DOWN_LINES: usize = 10;

// `#[tokio::main]` is a macro that generates a small synchronous `main`
// which starts a tokio runtime and immediately runs this `async fn` on
// it. The runtime is what `tokio::spawn` needs for the fetch and download
// tasks. Note that `run` itself never awaits anything: it polls the
// channels with `try_recv`.
#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::analyze();

    // Built before touching the terminal: if the HTTP client cannot be
    // created, the error is printed on a normal, un-broken terminal.
    let (dl_tx, mut dl_rx) = mpsc::unbounded_channel::<DownloadMessage>();
    let downloader = Downloader::new(dl_tx).map_err(io::Error::other)?;

    // Using a default CrosstermBackend to stdout terminal saving the
    // previous screen to be restored at the end
    let mut terminal = ratatui::init();

    // An unbounded mpsc channel: many producers (here, one per spawned
    // fetch task), one consumer (the main loop). This is how the
    // background network task in `fetch.rs` reports back to the UI
    // without either side blocking the other.
    let (tx, mut rx) = mpsc::unbounded_channel::<FetchMessage>();
    let mut app = App::new();
    spawn_fetch(tx.clone(), FetchRequest::Root(cli.url));

    let result = run(
        &mut terminal,
        &mut app,
        &mut rx,
        &tx,
        &mut dl_rx,
        &downloader,
    )
    .await;

    // Always restore the terminal, even if `run` returned an error —
    // otherwise the user's shell would stay in raw mode / on the alternate
    // screen.
    ratatui::restore();

    result
}

/// The main loop. Each iteration: draw the frame if the state changed,
/// apply the fetch and download results that are ready, then wait up to
/// 50 ms for a terminal event.
#[allow(
    clippy::unused_async,
    reason = "`run` never awaits; kept async for now"
)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<FetchMessage>,
    tx: &mpsc::UnboundedSender<FetchMessage>,
    dl_rx: &mut mpsc::UnboundedReceiver<DownloadMessage>,
    downloader: &Downloader,
) -> io::Result<()> {
    loop {
        // `terminal.draw` takes a closure that receives the `Frame` for
        // this iteration and is expected to render the whole UI into it
        // (see `ui::draw`). ratatui diffs the result against the
        // previous frame and only writes the cells that changed.
        if app.is_ui_dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            app.ui_is_clean();
        }

        // Non-blocking: if a background fetch has finished since the
        // last iteration, apply its result now. If nothing is ready,
        // `try_recv` returns immediately instead of waiting.
        // `while let` (not `if let`): drain everything that is ready, so
        // results never pile up at one message per 50ms iteration.
        while let Ok(message) = rx.try_recv() {
            app.apply_fetch_result(message);
        }

        while let Ok(message) = dl_rx.try_recv() {
            app.apply_download_result(message);
        }

        // Progress lives in atomics, not in messages: nothing tells the
        // loop it changed, so redraw on every iteration while a download runs.
        if app.active_downloads() > 0 {
            app.ui_is_dirty();
        }

        // crossterm's `event::poll` blocks for up to the given duration,
        // waiting for a terminal event (key press, resize, ...), and
        // returns `true` as soon as one is available (or `false` once
        // the timeout elapses with nothing). Using a short timeout
        // instead of blocking forever is what lets this loop also check
        // the fetch channel regularly, without a second thread and
        // without busy-waiting (the thread is asleep between polls).
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    // On most platforms a single physical key press only
                    // generates a `Press` event, but on terminals with the
                    // "kitty keyboard protocol" it can also generate
                    // `Repeat`/`Release`; filtering to `Press` keeps behavior
                    // consistent everywhere.
                    if key.kind == KeyEventKind::Press {
                        app.ui_is_dirty();
                        match key.code {
                            KeyCode::Char('d') => app.sort(&SortKey::Date),
                            KeyCode::Char('g') => {
                                if let Some(job) = app.download_selected() {
                                    downloader.spawn(job);
                                }
                            }
                            KeyCode::Char('n') => app.sort(&SortKey::Name),
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('r') => {
                                if let Some(request) = app.refresh_page() {
                                    spawn_fetch(tx.clone(), request);
                                }
                            }
                            KeyCode::Char('s') => app.sort(&SortKey::Size),
                            KeyCode::Down => app.select_next(1),
                            KeyCode::Up => app.select_previous(1),
                            KeyCode::PageDown => app.select_next(PAGE_UP_DOWN_LINES),
                            KeyCode::PageUp => app.select_previous(PAGE_UP_DOWN_LINES),
                            KeyCode::Enter => {
                                if let Some(request) = app.enter_selected() {
                                    spawn_fetch(tx.clone(), request);
                                }
                            }
                            KeyCode::Backspace => {
                                app.go_back();
                            }
                            _ => {}
                        }
                    }
                }
                Event::FocusGained | Event::Resize(_, _) => app.ui_is_dirty(),
                _ => {}
            }
        }
    }
}
