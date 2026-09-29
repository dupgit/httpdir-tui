//! Background file downloads.
//!
//! Same idea as `fetch.rs` (never block the UI thread), with two
//! differences that come from the nature of the work:
//!
//! - `httpdirectory` only lists directories; it has no download API and
//!   its internal client carries a *global* timeout that would abort any
//!   large file. So downloads use their own `reqwest` client, which only
//!   times out on connect and on *stalled reads*, never on total duration.
//! - Progress is published through shared atomics (`Progress`) instead of
//!   channel messages: the UI reads them once per frame, and the channel
//!   only carries the final outcome. No message flood, no backlog.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, Url};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Semaphore;

/// Be polite to the remote server: at most this many transfers at once,
/// the others wait for a permit.
const MAX_CONCURRENT: usize = 2;

type BoxError = Box<dyn Error + Send + Sync>;

/// Shared between the download task (writer) and the UI (reader).
#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    /// `0` means "unknown" (no `Content-Length` from the server).
    pub total: AtomicU64,
}

/// Everything a download task needs; built by `App::download_selected`.
pub struct Job {
    id: usize,
    url: Url,
    dest: PathBuf,
    progress: Arc<Progress>,
}

/// Outcome of a download, sent once when the task ends.
pub struct DownloadMessage {
    pub id: usize,
    pub result: Result<(), String>,
}

impl Job {
    /// `base` is the URL of the listing, `link` the entry's (possibly
    /// relative) link, `name` the file name announced by the listing.
    pub fn new(
        id: usize,
        base: &str,
        link: &str,
        name: &str,
        progress: Arc<Progress>,
    ) -> Result<Self, String> {
        let file_name = safe_file_name(name).ok_or_else(|| format!("unsafe file name {name:?}"))?;
        let url = Url::parse(base)
            .and_then(|base| base.join(link))
            .map_err(|err| err.to_string())?;
        Ok(Self {
            id,
            url,
            // Relative path: files land in the current working directory.
            dest: PathBuf::from(file_name),
            progress,
        })
    }
}

/// The file name comes from a remote HTML page, i.e. from untrusted data.
/// A name such as `../../.bashrc` must never be joined to a local path, so
/// only accept names that are exactly their own last path component.
fn safe_file_name(name: &str) -> Option<&str> {
    match Path::new(name).file_name()?.to_str() {
        Some(last) if last == name => Some(last),
        _ => None,
    }
}

/// Owns the shared HTTP client and the concurrency limit.
pub struct Downloader {
    client: Client,
    permits: Arc<Semaphore>,
    tx: UnboundedSender<DownloadMessage>,
}

impl Downloader {
    pub fn new(tx: UnboundedSender<DownloadMessage>) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            // Applies to each read, not to the whole transfer.
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            tx,
        })
    }

    /// Returns immediately; the transfer runs on its own tokio task.
    pub fn spawn(&self, job: Job) {
        let client = self.client.clone();
        let permits = Arc::clone(&self.permits);
        let tx = self.tx.clone();

        tokio::spawn(async move {
            // The semaphore is never closed, so `acquire` cannot fail.
            let _permit = permits.acquire_owned().await.ok();
            let result = download(&client, &job).await.map_err(|err| err.to_string());
            let _ = tx.send(DownloadMessage { id: job.id, result });
        });
    }
}

/// Streams the body to `<name>.part`, then renames it to `<name>` once
/// complete: a file with its final name is therefore always a whole file.
async fn download(client: &Client, job: &Job) -> Result<(), BoxError> {
    if fs::try_exists(&job.dest).await? {
        return Err("file already exists".into());
    }

    let mut response = client.get(job.url.clone()).send().await?.error_for_status()?;
    job.progress
        .total
        .store(response.content_length().unwrap_or(0), Relaxed);

    let mut part = job.dest.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);

    // A leftover `.part` from an interrupted run is simply overwritten.
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&part)
        .await?;

    let copied: Result<(), BoxError> = async {
        // `chunk()` needs no extra reqwest feature (unlike `bytes_stream`).
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            job.progress.downloaded.fetch_add(chunk.len() as u64, Relaxed);
        }
        file.flush().await?;
        Ok(())
    }
    .await;

    drop(file);
    match copied {
        Ok(()) => fs::rename(&part, &job.dest).await.map_err(Into::into),
        Err(err) => {
            // Do not leave truncated data behind.
            let _ = fs::remove_file(&part).await;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::safe_file_name;

    #[test]
    fn accepts_plain_names() {
        assert_eq!(safe_file_name("debian-12.qcow2"), Some("debian-12.qcow2"));
    }

    #[test]
    fn rejects_path_tricks() {
        for bad in ["../x", "a/b", "/etc/passwd", "..", ".", ""] {
            assert_eq!(safe_file_name(bad), None, "{bad:?}");
        }
    }
}
