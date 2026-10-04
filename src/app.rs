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

use crate::download::{DownloadMessage, Job, Progress};
use crate::fetch::{FetchMessage, FetchRequest};
use httpdirectory::httpdirectory::HttpDirectory;
use httpdirectory::httpdirectoryentry::HttpDirectoryEntry;
use std::sync::Arc;

/// What to show in the status bar, and (indirectly) whether a fetch is
/// currently in flight.
pub enum Status {
    Loading,
    Ready,
    Error(String),
}

/// Lifecycle of one download, as shown in the downloads panel.
pub enum DownloadState {
    Running,
    Done,
    Failed(String),
}

/// One line of the downloads panel. `progress` is shared with the
/// background task, which updates it while the UI only reads it.
pub struct Download {
    pub name: String,
    pub progress: Arc<Progress>,
    pub state: DownloadState,
}

#[derive(Clone)]
pub enum SortKey {
    Date,
    Name,
    Size,
}

pub struct App {
    /// Listing currently displayed, `None` only before the very first
    /// fetch completes.
    pub current: Option<HttpDirectory>,
    /// Previously visited listings, most recent last. Going back pops this
    /// stack instead of re-fetching the parent directory: it is already
    /// known, so there is no reason to ask the server for it again. A
    /// listing is only pushed once its successor has actually loaded (see
    /// `apply_fetch_result`), so a failed navigation leaves it untouched.
    history: Vec<HttpDirectory>,
    /// Index into `entries()` of the currently highlighted row. Handed
    /// to ratatui's `ListState` each frame in `ui::draw_entries`.
    pub selected: usize,
    pub status: Status,
    pub ordering: Option<(bool, SortKey)>,
    pub header: Vec<String>,
    /// Every download started during this session, oldest first. The
    /// index in this vector is the id used by `download::DownloadMessage`.
    pub downloads: Vec<Download>,

    /// true when something changed in the application that needs to
    /// to draw it's frame again
    pub is_ui_dirty: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            current: None,
            history: Vec::new(),
            selected: 0,
            status: Status::Loading,
            ordering: None,
            header: vec![
                "Type".to_string(),
                "Date".to_string(),
                "Name".to_string(),
                "Size".to_string(),
            ],
            downloads: Vec::new(),
            is_ui_dirty: true,
        }
    }

    /// The terminal frame has just been redraw so
    /// it is marked as clean (not dirty)
    pub fn ui_is_clean(&mut self) {
        self.is_ui_dirty = false;
    }

    /// The application state has changed and the ui
    /// needs to be redrawned to reflect this change
    /// so it is marked as dirty
    pub fn ui_is_dirty(&mut self) {
        self.is_ui_dirty = true;
    }

    pub fn sort_consistent(&mut self, dir: HttpDirectory) -> HttpDirectory {
        if let Some((ascending, ref sort_key)) = self.ordering {
            match sort_key {
                SortKey::Date => dir.sort_by_date(ascending),
                SortKey::Name => dir.sort_by_name(ascending),
                SortKey::Size => dir.sort_by_size(ascending),
            }
        } else {
            dir
        }
    }

    /// Called from the main loop whenever a background fetch (see
    /// `fetch.rs`) has produced a result. This is the *only* place
    /// `current`/`status` change in response to network activity.
    pub fn apply_fetch_result(&mut self, message: FetchMessage) {
        match message {
            FetchMessage::Loaded(mut dir) => {
                // Sorting the new httpdirectory to keep the ordering
                // consistent along the navigation
                dir = self.sort_consistent(dir);
                // The listing we are leaving becomes the one "back" returns
                // to. Doing it here, on success only, is what keeps
                // `history` consistent when a navigation fails. At startup
                // there is no previous listing, so nothing is pushed.
                if let Some(previous) = self.current.replace(dir) {
                    self.history.push(previous);
                }
                self.selected = 0;
                self.status = Status::Ready;
            }
            // Deliberately kept minimal: on error we keep the previous
            // listing on screen (if any) rather than clearing it, so a
            // failed navigation attempt does not lose the user's place.
            // `enter_selected` leaves `current` and `history` untouched
            // for exactly this reason.
            FetchMessage::Failed(err) => {
                self.status = Status::Error(err);
            }
        }
        self.ui_is_dirty();
    }

    /// Entries of the currently displayed listing, or an empty slice
    /// while nothing has loaded yet. Centralizing this (instead of
    /// letting callers match on `Option<HttpDirectory>` themselves) is
    /// what lets `ui.rs` treat "no data yet" and "empty directory" the
    /// same way: just an empty list.
    pub fn entries(&self) -> &[HttpDirectoryEntry] {
        self.current
            .as_ref()
            .map_or(&[], |dir| dir.entries().as_slice())
    }

    pub fn select_next(&mut self, length: usize) {
        let len = self.entries().len();
        if len > 0 {
            self.selected = (self.selected + length) % len;
        }
    }

    pub fn select_previous(&mut self, length: usize) {
        let len = self.entries().len();
        if len > 0 {
            self.selected = (self.selected + len - length) % len;
        }
    }

    /// Builds the request needed to descend into the selected entry, if it
    /// is a directory (or the parent-directory marker). Returns `None` for
    /// files: this skeleton only browses, it does not download or open
    /// anything -- add that deliberately, rather than by accident.
    pub fn enter_selected(&mut self) -> Option<FetchRequest> {
        // One navigation at a time: while a fetch is in flight, its result
        // is what decides what `current` and `history` become.
        if matches!(self.status, Status::Loading) {
            return None;
        }
        let entry = self.entries().get(self.selected)?;
        if entry.is_file() {
            return None;
        }
        let link = entry_link(entry).to_string();

        // `HttpDirectory::cd` consumes its receiver and drops it on error,
        // so the task works on a clone: `current` stays displayed, and
        // stays the place to come back to, until the new listing arrives.
        let current = self.current.clone()?;
        self.status = Status::Loading;
        Some(FetchRequest::Cd(current, link))
    }

    /// Builds the job needed to download the selected entry, if it is a
    /// file and is not already being downloaded. Returns `None` otherwise.
    /// Invalid entries (e.g. a hostile file name) are reported in the
    /// downloads panel instead of being silently ignored.
    pub fn download_selected(&mut self) -> Option<Job> {
        let entry = self.entries().get(self.selected)?;
        if !entry.is_file() {
            return None;
        }
        let name = entry.filename()?.to_string();
        let link = entry_link(entry);
        let base = self.current.as_ref()?.get_url();

        // Pressing the key twice must not start two writers on one file.
        let already_running = self
            .downloads
            .iter()
            .any(|d| d.name == name && matches!(d.state, DownloadState::Running));
        if already_running {
            return None;
        }

        let progress = Arc::new(Progress::default());
        let id = self.downloads.len();
        let (job, state) = match Job::new(id, &base, link, &name, Arc::clone(&progress)) {
            Ok(job) => (Some(job), DownloadState::Running),
            Err(err) => (None, DownloadState::Failed(err)),
        };
        self.downloads.push(Download {
            name,
            progress,
            state,
        });
        job
    }

    /// Records the final outcome of a download task.
    pub fn apply_download_result(&mut self, message: DownloadMessage) {
        self.ui_is_dirty();
        if let Some(download) = self.downloads.get_mut(message.id) {
            download.state = match message.result {
                Ok(()) => DownloadState::Done,
                Err(err) => DownloadState::Failed(err),
            };
        }
    }

    pub fn active_downloads(&self) -> usize {
        self.downloads
            .iter()
            .filter(|d| matches!(d.state, DownloadState::Running))
            .count()
    }

    /// Sorts the current listing by the appropriate method which from
    /// `httpdirectory` takes `self` by value and hands back a sorted
    /// `Self` -- it is a plain, synchronous, in-memory operation on
    /// already-fetched entries, so no fetch task/channel round-trip is
    /// needed here: it can run directly on the UI thread.
    pub fn sort(&mut self, sort: &SortKey) {
        let ascending = if self.ordering.is_none() {
            false
        } else if let Some((ascending, _)) = self.ordering {
            !ascending
        } else {
            false
        };

        self.ordering = Some((ascending, sort.clone()));

        if let Some(dir) = self.current.take() {
            match sort {
                SortKey::Date => {
                    self.current = Some(dir.sort_by_date(ascending));
                    self.add_ordering_char(ascending, 1);
                }
                SortKey::Name => {
                    self.current = Some(dir.sort_by_name(ascending));
                    self.add_ordering_char(ascending, 2);
                }
                SortKey::Size => {
                    self.current = Some(dir.sort_by_size(ascending));
                    self.add_ordering_char(ascending, 3);
                }
            }
            // The old `selected` index may no longer point at the same
            // entry now that the order changed; resetting it avoids
            // silently highlighting an unrelated row.
            self.selected = 0;
        }
    }

    fn add_ordering_char(&mut self, ascending: bool, index: usize) {
        let ordering = if ascending { '▴' } else { '▾' };

        self.header = vec![
            "Type".to_string(),
            "Date".to_string(),
            "Name".to_string(),
            "Size".to_string(),
        ];
        self.header[index] = format!("{} {ordering}", self.header[index]);
    }

    /// Restores the previous listing from history, with no network call.
    /// Returns `true` if there was something to go back to.
    pub fn go_back(&mut self) -> bool {
        // Not while a fetch is in flight: its result would then be pushed
        // onto a history that no longer matches what is on screen.
        if matches!(self.status, Status::Loading) {
            return false;
        }
        match self.history.pop() {
            Some(previous) => {
                self.current = Some(self.sort_consistent(previous));
                self.selected = 0;
                self.status = Status::Ready;
                true
            }
            None => false,
        }
    }
}

/// Extracts the link of an entry, whatever its kind. Callers decide what
/// to do with it: `enter_selected` follows directories, `download_selected`
/// fetches files.
fn entry_link(entry: &HttpDirectoryEntry) -> &str {
    match entry {
        HttpDirectoryEntry::ParentDirectory(link) => link,
        HttpDirectoryEntry::Directory(inner) | HttpDirectoryEntry::File(inner) => inner.link(),
    }
}
