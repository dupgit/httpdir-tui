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
//!
//! Interrupted downloads are resumed. While a transfer is in flight, the
//! data lives in `<name>.part` and the identity of the remote version
//! (its `ETag`, or failing that its `Last-Modified`) in `<name>.part.meta`.
//! Both are kept when a transfer fails or the program quits; pressing the
//! download key again sends `Range` + `If-Range`, so the server only
//! continues if the file is still the very same one. Otherwise it answers
//! with the whole file and we start over: a resume can never glue the head
//! of one version to the tail of another.

use reqwest::header::{CONTENT_RANGE, ETAG, HeaderValue, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, Response, StatusCode, Url};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

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
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
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

/// What an interrupted run left behind and can safely be continued from.
struct Partial {
    len: u64,
    /// `ETag` or `Last-Modified` of the version the bytes in `.part` come from.
    validator: HeaderValue,
}

/// Streams the body to `<name>.part`, then renames it to `<name>` once
/// complete: a file with its final name is therefore always a whole file.
/// On failure, `.part` and `.part.meta` are kept so the next attempt can
/// resume.
async fn download(client: &Client, job: &Job) -> Result<(), BoxError> {
    if fs::try_exists(&job.dest).await? {
        return Err("file already exists".into());
    }

    let part = with_suffix(&job.dest, ".part");
    let meta = with_suffix(&job.dest, ".part.meta");

    let partial = read_partial(&part, &meta).await;
    let (mut response, start) = request(client, &job.url, partial.as_ref()).await?;

    // For a resumed transfer the body only holds the missing tail.
    let total = response.content_length().map_or(0, |len| start + len);
    job.progress.total.store(total, Relaxed);
    job.progress.downloaded.store(start, Relaxed);

    let mut options = OpenOptions::new();
    if start > 0 {
        options.append(true);
    } else {
        options.write(true).create(true).truncate(true);
    }
    let mut file = options.open(&part).await?;

    // Order matters: `.part` is truncated *before* the new validator is
    // written. Whatever a crash leaves behind, the validator on disk is
    // never newer than the bytes it vouches for.
    if start == 0 {
        match validator_of(&response) {
            Some(validator) => fs::write(&meta, validator).await?,
            // Nothing to prove the next attempt talks to the same file: not resumable.
            None => {
                let _ = fs::remove_file(&meta).await;
            }
        }
    }

    let copied = async {
        // `chunk()` needs no extra reqwest feature (unlike `bytes_stream`).
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
            job.progress
                .downloaded
                .fetch_add(chunk.len() as u64, Relaxed);
        }
        Ok::<(), BoxError>(())
    }
    .await;

    // Flush even after a failure: what reached the disk is what a later
    // resume builds upon.
    let flushed = file.flush().await;
    drop(file);
    copied?;
    flushed?;

    let done = job.progress.downloaded.load(Relaxed);
    if total > 0 && done != total {
        return Err(format!("incomplete download: {done} of {total} bytes").into());
    }

    fs::rename(&part, &job.dest).await?;
    let _ = fs::remove_file(&meta).await;
    Ok(())
}

/// Asks for the whole file, or only the missing tail when `partial` is
/// usable. Returns the response and the offset its body starts at.
async fn request(
    client: &Client,
    url: &Url,
    partial: Option<&Partial>,
) -> Result<(Response, u64), BoxError> {
    if let Some(partial) = partial {
        let response = client
            .get(url.clone())
            .header(RANGE, format!("bytes={}-", partial.len))
            .header(IF_RANGE, partial.validator.clone())
            .send()
            .await?;
        match response.status() {
            // Range honoured and file unchanged. Check the server really
            // starts where we asked: appending anywhere else corrupts.
            StatusCode::PARTIAL_CONTENT if range_start(&response) == Some(partial.len) => {
                return Ok((response, partial.len));
            }
            StatusCode::PARTIAL_CONTENT => {
                return Err("server sent an unexpected Content-Range".into());
            }
            // `.part` is not a valid prefix (e.g. longer than the remote
            // file): fall through and start over.
            StatusCode::RANGE_NOT_SATISFIABLE => {}
            // 200: the file changed (`If-Range` failed) or the server does
            // not do ranges. Either way the whole body follows.
            _ => return Ok((response.error_for_status()?, 0)),
        }
    }
    let response = client.get(url.clone()).send().await?.error_for_status()?;
    Ok((response, 0))
}

/// `.part` is only worth resuming if it is not empty and we know which
/// remote version it came from.
async fn read_partial(part: &Path, meta: &Path) -> Option<Partial> {
    let len = fs::metadata(part).await.ok()?.len();
    let validator = fs::read_to_string(meta).await.ok()?;
    let validator = validator.trim();
    if len == 0 || validator.is_empty() {
        return None;
    }
    Some(Partial {
        len,
        validator: HeaderValue::from_str(validator).ok()?,
    })
}

/// Identifies the remote version. `If-Range` only accepts a *strong* `ETag`,
/// so a weak one (`W/"..."`) is skipped in favour of `Last-Modified`.
/// See [ETag mozilla's documentation](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/ETag)
fn validator_of(response: &Response) -> Option<String> {
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    header(ETAG)
        .filter(|etag| !etag.starts_with("W/"))
        .or_else(|| header(LAST_MODIFIED))
}

fn range_start(response: &Response) -> Option<u64> {
    parse_range_start(response.headers().get(CONTENT_RANGE)?.to_str().ok()?)
}

/// `bytes 1000-1999/2000` -> `1000`. `bytes */2000` (a 416 answer) -> `None`.
fn parse_range_start(value: &str) -> Option<u64> {
    value
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::{parse_range_start, safe_file_name, with_suffix};
    use std::path::Path;

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

    #[test]
    fn reads_range_start() {
        assert_eq!(parse_range_start("bytes 1000-1999/2000"), Some(1000));
        assert_eq!(parse_range_start("bytes 0-9/*"), Some(0));
        assert_eq!(parse_range_start("bytes */2000"), None);
        assert_eq!(parse_range_start("items 1-2/3"), None);
    }

    #[test]
    fn appends_suffix_to_the_whole_name() {
        assert_eq!(
            with_suffix(Path::new("a.tar.gz"), ".part"),
            Path::new("a.tar.gz.part")
        );
    }
}
