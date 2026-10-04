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
mod download;
mod fetch;
mod ui;
use app::{App, SortKey};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use download::{DownloadMessage, Downloader};
use fetch::{FetchMessage, FetchRequest, spawn_fetch};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_URL: &str = "https://cloud.debian.org/images/cloud/";

// `#[tokio::main]` is a macro that generates a small synchronous `main`
// which starts a tokio runtime and immediately runs this `async fn` on
// it. It's what lets us `.await` things (network calls, channel receives)
// directly in `main` and `run`.
#[tokio::main]
async fn main() -> io::Result<()> {
    let start_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_URL.to_string());

    // Built before touching the terminal: if the HTTP client cannot be
    // created, the error is printed on a normal, un-broken terminal.
    let (dl_tx, mut dl_rx) = mpsc::unbounded_channel::<DownloadMessage>();
    let downloader = Downloader::new(dl_tx).map_err(io::Error::other)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // `Terminal<CrosstermBackend<...>>` is ratatui's handle for drawing:
    // the backend (crossterm) knows how to move the cursor and write
    // styled text; `Terminal` adds frame buffering and diffing on top,
    // so only cells that actually changed are re-sent to the terminal.
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    // An unbounded mpsc channel: many producers (here, one per spawned
    // fetch task), one consumer (the main loop). This is how the
    // background network task in `fetch.rs` reports back to the UI
    // without either side blocking the other.
    let (tx, mut rx) = mpsc::unbounded_channel::<FetchMessage>();
    let mut app = App::new();
    spawn_fetch(tx.clone(), FetchRequest::Root(start_url));

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
    // otherwise a crash would leave the user's shell in raw mode /
    // stuck on the alternate screen.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// The main loop. Each iteration: draw one frame, check for a finished
/// background fetch, then wait (briefly) for a key press.
#[allow(clippy::unused_async, reason = "Clippy false detection")]
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
        // results never pile up at one message per 100ms iteration.
        while let Ok(message) = rx.try_recv() {
            app.apply_fetch_result(message);
        }

        while let Ok(message) = dl_rx.try_recv() {
            app.apply_download_result(message);
        }

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
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('s') => app.sort(&SortKey::Size),
                            KeyCode::Char('d') => app.sort(&SortKey::Date),
                            // Shift+d: only one modifier away from the sort key
                            // above, hence the `Char` match on the capital.
                            KeyCode::Char('g') => {
                                if let Some(job) = app.download_selected() {
                                    downloader.spawn(job);
                                }
                            }
                            KeyCode::Char('n') => app.sort(&SortKey::Name),
                            KeyCode::Down => app.select_next(),
                            KeyCode::Up => app.select_previous(),
                            KeyCode::Enter => {
                                if let Some(request) = app.enter_selected() {
                                    spawn_fetch(tx.clone(), request);
                                }
                            }
                            KeyCode::Backspace | KeyCode::Left => {
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
