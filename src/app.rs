//! Application state.
//!
//! ratatui is an *immediate-mode* UI library: `ui::draw` rebuilds every
//! widget from scratch on every frame, and nothing about a widget
//! survives between frames on its own. That means all the state that
//! *does* need to persist (which listing is shown, which row is
//! selected, is a fetch in flight...) has to live somewhere else — this
//! module. `ui::draw` only ever reads from `App`; `main.rs` is the only
//! place that mutates it, in response to input events or fetch results.
//! This "one owner mutates, everyone else reads" split is what keeps the
//! rendering code simple.

use crate::fetch::{FetchMessage, FetchRequest};
use httpdirectory::httpdirectory::HttpDirectory;
use httpdirectory::httpdirectoryentry::HttpDirectoryEntry;

/// What to show in the status bar, and (indirectly) whether a fetch is
/// currently in flight.
pub enum Status {
    Loading,
    Ready,
    Error(String),
}

pub enum Ordering {
    None,
    Ascending,
    Descending,
}

pub struct App {
    /// Listing currently displayed, `None` only before the very first
    /// fetch completes.
    pub current: Option<HttpDirectory>,
    /// Previously visited listings, most recent last. Going back pops this
    /// stack instead of re-fetching the parent directory: it is already
    /// known, so there is no reason to ask the server for it again.
    history: Vec<HttpDirectory>,
    /// Index into `entries()` of the currently highlighted row. Handed
    /// to ratatui's `ListState` each frame in `ui::draw_entries`.
    pub selected: usize,
    pub status: Status,
    pub ordering: Ordering,
    pub header: Vec<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            current: None,
            history: Vec::new(),
            selected: 0,
            status: Status::Loading,
            ordering: Ordering::None,
            header: vec![
                "Type".to_string(),
                "Date".to_string(),
                "Name".to_string(),
                "Size".to_string(),
            ],
        }
    }

    /// Called from the main loop whenever a background fetch (see
    /// `fetch.rs`) has produced a result. This is the *only* place
    /// `current`/`status` change in response to network activity.
    pub fn apply_fetch_result(&mut self, message: FetchMessage) {
        match message {
            FetchMessage::Loaded(dir) => {
                self.current = Some(dir);
                self.selected = 0;
                self.status = Status::Ready;
            }
            // Deliberately kept minimal: on error we keep the previous
            // listing on screen (if any) rather than clearing it, so a
            // failed navigation attempt does not lose the user's place.
            FetchMessage::Failed(err) => {
                self.status = Status::Error(err);
            }
        }
    }

    /// Entries of the currently displayed listing, or an empty slice
    /// while nothing has loaded yet. Centralizing this (instead of
    /// letting callers match on `Option<HttpDirectory>` themselves) is
    /// what lets `ui.rs` treat "no data yet" and "empty directory" the
    /// same way: just an empty list.
    pub fn entries(&self) -> &[HttpDirectoryEntry] {
        self.current
            .as_ref()
            .map(|dir| dir.entries().as_slice())
            .unwrap_or(&[])
    }

    pub fn select_next(&mut self) {
        let len = self.entries().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn select_previous(&mut self) {
        let len = self.entries().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    /// Builds the request needed to descend into the selected entry, if it
    /// is a directory (or the parent-directory marker). Returns `None` for
    /// files: this skeleton only browses, it does not download or open
    /// anything -- add that deliberately, rather than by accident.
    pub fn enter_selected(&mut self) -> Option<FetchRequest> {
        let entry = self.entries().get(self.selected)?;
        if entry.is_file() {
            return None;
        }
        let link = entry_link(entry)?.to_string();

        let current = self.current.take()?;
        self.history.push(current.clone());
        self.status = Status::Loading;
        Some(FetchRequest::Cd(current, link))
    }

    /// Changes ordering order each time called
    fn swap_ordering(&mut self) {
        match self.ordering {
            Ordering::Ascending => self.ordering = Ordering::Descending,
            Ordering::Descending => self.ordering = Ordering::Ascending,
            Ordering::None => self.ordering = Ordering::Ascending,
        }
    }

    /// Sorts the current listing by the appropriate method which from
    /// `httpdirectory` takes `self` by value and hands back a sorted
    /// `Self` -- it is a plain, synchronous, in-memory operation on
    /// already-fetched entries, so no fetch task/channel round-trip is
    /// needed here: it can run directly on the UI thread.
    pub fn sort<F>(&mut self, f: F) -> Ordering
    where
        F: Fn(HttpDirectory, bool) -> HttpDirectory,
    {
        let mut ordering = Ordering::None;
        if let Some(dir) = self.current.take() {
            self.swap_ordering();
            match self.ordering {
                Ordering::None => self.current = Some(dir),
                Ordering::Ascending => {
                    self.current = Some(f(dir, true));
                    ordering = Ordering::Ascending;
                }
                Ordering::Descending => {
                    self.current = Some(f(dir, false));
                    ordering = Ordering::Descending;
                }
            }
            // The old `selected` index may no longer point at the same
            // entry now that the order changed; resetting it avoids
            // silently highlighting an unrelated row.
            self.selected = 0;
        }

        ordering
    }

    fn add_ordering_char(&mut self, order: Ordering, index: usize) {
        let ordering = match order {
            Ordering::Ascending => '▴',
            Ordering::Descending => '▾',
            _ => ' ',
        };

        self.header = vec![
            "Type".to_string(),
            "Date".to_string(),
            "Name".to_string(),
            "Size".to_string(),
        ];
        self.header[index] = format!("{} {ordering}", self.header[index]);
    }

    pub fn sort_by_size(&mut self) {
        let order = self.sort(HttpDirectory::sort_by_size);
        self.add_ordering_char(order, 3);
    }

    pub fn sort_by_date(&mut self) {
        let order = self.sort(HttpDirectory::sort_by_date);
        self.add_ordering_char(order, 1);
    }

    pub fn sort_by_name(&mut self) {
        let order = self.sort(HttpDirectory::sort_by_name);
        self.add_ordering_char(order, 2);
    }

    /// Restores the previous listing from history, with no network call.
    /// Returns `true` if there was something to go back to.
    pub fn go_back(&mut self) -> bool {
        match self.history.pop() {
            Some(previous) => {
                self.current = Some(previous);
                self.selected = 0;
                self.status = Status::Ready;
                true
            }
            None => false,
        }
    }
}

/// Extracts the link to follow for an entry, whatever its kind.
fn entry_link(entry: &HttpDirectoryEntry) -> Option<&str> {
    match entry {
        HttpDirectoryEntry::ParentDirectory(link) => Some(link),
        HttpDirectoryEntry::Directory(inner) => Some(inner.link()),
        HttpDirectoryEntry::File(_) => None,
    }
}
