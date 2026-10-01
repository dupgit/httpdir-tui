# httpdir-tui

A terminal browser for HTTP directory indexes, with background downloads
that can be resumed.

Many mirrors and software archives publish their files as plain "index of"
pages (Apache `mod_autoindex`, nginx `autoindex`, h5ai, miniserve, ...).
`httpdir-tui` lets you walk through them like a file manager, sort them,
and download the files you need, without leaving the terminal or opening a
browser. It is built on [ratatui](https://ratatui.rs) and on the
[`httpdirectory`](https://crates.io/crates/httpdirectory) crate, which does
the actual parsing of the index pages.

## Features

- **Browse** a remote directory index: type, date, name and size of every
  entry, `..` included. Any index format understood by `httpdirectory` works.
- **Sort** the current listing by name, date or size, ascending or
  descending, with an arrow in the column header.
- **Go back instantly**: previously visited directories are kept in memory,
  so going up again never hits the network.
- **Download in the background** while you keep browsing. A panel shows the
  most recent transfers with their progress.
- **Resume interrupted downloads**: after a failure or a quit, pressing the
  download key again continues where the transfer stopped, as long as the
  server can prove the file has not changed (see below).
- **Stay responsive** whatever the speed of the server: the interface never
  waits for the network.

## Requirements

Rust 1.88 or later (required by `httpdirectory`).

## Run

```
cargo run --release -- https://cloud.debian.org/images/cloud/
```

The URL is optional: without it, the Debian cloud images index above is
opened. Downloaded files are written to the directory you launched the
program from.

## Keys

| Key                  | Action                                              |
|----------------------|-----------------------------------------------------|
| `Up` / `Down`        | Move the selection                                  |
| `Enter`              | Open the selected directory (or `..`)               |
| `Backspace` / `Left` | Go back to the previous directory (no network call) |
| `n` / `d` / `s`      | Sort by name / date / size; press again to reverse  |
| `g`                  | Download the selected file                           |
| `q` / `Esc`          | Quit                                                |

## Downloads

- A file is first written to `<name>.part` and renamed to `<name>` only once
  it is complete: a file with its final name is always a whole file.
- An existing file is never overwritten.
- At most two transfers run at the same time; the others wait their turn, to
  stay polite with the remote server.
- Names announced by the remote page are untrusted data: a name that is not
  a plain file name (such as `../../.bashrc`) is refused and reported in the
  panel.
- Large files are not cut off: only the connection and stalled reads time
  out, never the total duration of a transfer.

### Resuming

While a transfer runs, `<name>.part.meta` records which version of the
remote file the data comes from (its `ETag`, or its `Last-Modified`). If the
transfer fails, or if you quit, both files are kept. The next download of the
same file asks the server for the missing part only, and the server continues
only if the file is still the same one; otherwise it sends the whole file and
the download starts over. A resume therefore never glues the beginning of one
version to the end of another.

If the server provides no usable validator, or does not support range
requests, the file is simply downloaded again from the start. To force a
fresh start, delete the `.part` file.

## Architecture

- `src/fetch.rs`: all directory listing requests, each in its own
  `tokio::spawn` task. The UI never `await`s an HTTP request; it reads
  results from a channel.
- `src/download.rs`: background downloads, with their own HTTP client (the
  one inside `httpdirectory` has a global timeout and is not exposed).
  Progress is shared through atomics read by the UI; the channel only carries
  the final outcome.
- `src/app.rs`: application state: current listing, history stack,
  selection, sort order, downloads. It is mutated in one place only.
- `src/ui.rs`: ratatui rendering (URL header, entry table, downloads panel,
  help bar). It only reads the state.
- `src/main.rs`: terminal setup, event loop and wiring of the modules above.
  The screen is redrawn only when something changed.

## Deliberately not done

- No prefetching of the tree: each level is loaded on demand, which limits
  the number of requests sent to the server.
- No re-request of a directory already visited: going back pops a local
  history stack.
- No automatic retries: a failed transfer waits for you to press `g` again,
  rather than hammering a struggling server.
- No opening of files: `Enter` only descends into directories.

## Known limitations

- Quitting while a download is running abandons it. The `.part` and
  `.part.meta` files stay on disk so that it can be resumed next time.
- Downloads always go to the launch directory; there is no destination
  setting yet.
