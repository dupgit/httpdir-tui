//! Rendering. This is the only file that talks to ratatui's *widget* API.
//!
//! ratatui's rendering model, in a nutshell:
//! - Every frame, the whole UI is redrawn from scratch (immediate-mode,
//!   like a game engine): there is no persistent widget tree to mutate,
//!   just a `draw(...)` function called on every loop iteration.
//! - A `Frame` represents "the terminal, right now". You never write to
//!   the screen directly; you build widgets and hand them to the frame.
//! - `Rect` is a rectangle (x, y, width, height) in terminal cells.
//!   `Layout` computes a set of `Rect`s from constraints, so you almost
//!   never compute pixel/cell coordinates by hand.
//! - A "widget" (`Paragraph`, `Table`, ...) is a small, cheap, throwaway
//!   value: you construct one, render it into a `Rect`, and drop it.
//!   Nothing about it survives between frames except the *state* you
//!   choose to keep yourself (here, in `App`).

use httpdirectory::httpdirectoryentry::HttpDirectoryEntry;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::app::{App, Status};

/// Entry point called once per frame from the main loop (`terminal.draw`).
/// `frame` is where widgets get rendered; `app` is read-only here — this
/// function only *displays* state, it never changes it.
pub fn draw(frame: &mut Frame, app: &App) {
    // `frame.area()` is the full terminal Rect available this frame (it
    // changes automatically if the user resizes the terminal window).
    let area = frame.area();

    // help message is 96 characters wide so use two lines (4 because
    // we have top and bottom borders) when the terminal is smaller
    // than that.
    let help = if area.width > 96 { 3 } else { 4 };

    // `Layout::vertical([...])` splits one Rect into several, stacked
    // top to bottom, sized by the given constraints:
    //   - `Length(n)`: exactly n rows.
    //   - `Min(0)`: "take whatever is left" — used for the middle panel
    //     so it grows/shrinks with the terminal size.
    // The result is a Vec<Rect> in the same order as the constraints.
    let chunks = Layout::vertical([
        Constraint::Length(3), // header: bordered box, needs 3 rows (1 content + 2 border)
        Constraint::Min(1),    // entry table: fills all remaining space
        Constraint::Length(help), // status line: a single row, no border
    ])
    .split(area);

    draw_header(frame, chunks[0], app);
    draw_entries(frame, chunks[1], app);
    draw_status_bar(frame, chunks[2], app);
}

/// A `Paragraph` is ratatui's plain-text widget. Wrapping it in a `Block`
/// (with `Borders::ALL` and a `title`) draws a box around it — the Block
/// is itself a widget, and `Paragraph::block(...)` nests one inside it.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let url = app
        .current
        .as_ref()
        .map(|dir| dir.get_url().to_string())
        .unwrap_or_else(|| "...".to_string());

    // Title is the text that comes along with the border
    let title = Span::styled(" URL ", Style::default().bold());

    let paragraph = Paragraph::new(format!(" {url}")).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().yellow()),
    );

    // `render_widget` consumes the widget and draws it into `area`. This
    // is the "stateless" rendering call: the widget has no memory of
    // previous frames.
    frame.render_widget(paragraph, area);
}

/// A `Table` needs a companion `TableState` to know which row is
/// highlighted — this is ratatui's *stateful* widget pattern: state that
/// must persist across frames (like "which row is selected") lives
/// outside the widget and is passed in by `&mut` reference each time.
/// Here we rebuild a fresh `TableState` every frame from `app.selected`,
/// which is simple and correct for a one-way flow (App -> UI); ratatui
/// also allows keeping the `TableState` itself as persistent app state if
/// you want the widget to own scroll offsets etc.
fn draw_entries(frame: &mut Frame, area: Rect, app: &App) {
    // The header row: plain text cells, bolded so it stands out from the
    // data rows below it.
    // let header = Row::new(["Type", "Date", "Name", "Size"]).style(Style::default().bold());
    let header = Row::new(app.header.to_owned()).style(Style::default().bold());

    // Turn each domain entry into a `Row` of `Cell`s. ratatui widgets are
    // built from iterators/Vecs of smaller widgets like this throughout
    // the crate: Table is made of Rows, a Row is made of Cells.
    let rows: Vec<Row> = app.entries().iter().map(entry_row).collect();

    // Column widths, same mini-language as `Layout` constraints above:
    // fixed widths for the short, predictable columns, and `Min` for the
    // name column so it absorbs any extra terminal width.
    let widths = [
        Constraint::Length(5),  // Type: "DIR", "FILE", ".."
        Constraint::Length(17), // Date: " YYYY-MM-DD HH:MM "
        Constraint::Min(20),    // Name: grows with the terminal
        Constraint::Length(10), // Size
    ];

    let title = Span::styled(" Contents ", Style::default().bold());
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().yellow()),
        )
        // Prefix shown in front of the selected row.
        .highlight_symbol("☞ ")
        // Style applied to the selected row; `REVERSED` swaps
        // foreground/background so it stands out without picking
        // explicit colors (which keeps it readable in any terminal
        // theme, light or dark).
        .row_highlight_style(Style::default().reversed());

    let mut state = TableState::default();
    if !app.entries().is_empty() {
        // `select(Some(i))` tells the widget which row to highlight and
        // to auto-scroll into view if needed. `None` means "no selection".
        state.select(Some(app.selected));
    }

    // Stateful rendering: unlike `render_widget`, this call takes the
    // state by `&mut` so the widget can also write back into it (e.g.
    // ratatui may adjust an internal scroll offset here).
    frame.render_stateful_widget(table, area, &mut state);
}

/// One line of plain text, no border: a minimal status/help bar.
fn draw_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let title = Span::styled(" Help ", Style::default().bold());
    let text = match &app.status {
        Status::Loading => "Loading...".to_string(),
        Status::Ready => {
            let separator = if area.width > 96 { '|' } else { '\n' };
            format!(" Up/Down: move | Enter: open | Backspace: back {separator} n, d, s: sort by name, date, size | Esc: quit")
        }
        Status::Error(err) => format!("Error: {err}"),
    };

    let paragraph = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().yellow()),
    );

    frame.render_widget(paragraph, area);
}

/// Maps one domain value (`HttpDirectoryEntry`, from the `httpdirectory`
/// crate) to one display value (a table `Row`). Keeping this conversion
/// in its own function is what keeps `app.rs` free of any ratatui
/// import: the domain model doesn't need to know how it's drawn.
fn entry_row(entry: &HttpDirectoryEntry) -> Row<'static> {
    Row::new([
        Cell::from(entry_type_label(entry)),
        Cell::from(entry_date_label(entry)),
        Cell::from(entry_name_label(entry)),
        Cell::from(entry_size_label(entry)),
    ])
}

fn entry_type_label(entry: &HttpDirectoryEntry) -> &'static str {
    match entry {
        HttpDirectoryEntry::ParentDirectory(_) => "DIR",
        HttpDirectoryEntry::Directory(_) => "DIR",
        HttpDirectoryEntry::File(_) => "FILE",
    }
}

fn entry_name_label(entry: &HttpDirectoryEntry) -> String {
    // `HttpDirectoryEntry::name()` already handles all three variants for
    // us (it returns `None` only for `ParentDirectory`), so there is no
    // need to `match` again here.
    entry.name().unwrap_or("..").to_string()
}

fn entry_date_label(entry: &HttpDirectoryEntry) -> String {
    // `entry.date()` returns `chrono::NaiveDateTime`; `.format(...)` is
    // an inherent method on that type, so no extra dependency on chrono
    // is needed in this crate just to call it.
    entry
        .date()
        .map(|date| date.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn entry_size_label(entry: &HttpDirectoryEntry) -> String {
    // Size lives on `Entry`, not on the `HttpDirectoryEntry` enum itself,
    // so this one still needs an explicit match to reach it — it is only
    // meaningful for files; directories and the parent marker show "-".
    match entry {
        HttpDirectoryEntry::File(inner) => inner.apparent_size().to_string(),
        HttpDirectoryEntry::Directory(_) | HttpDirectoryEntry::ParentDirectory(_) => {
            "-".to_string()
        }
    }
}
