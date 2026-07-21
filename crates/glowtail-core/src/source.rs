use crate::events::LogEvent;
use crate::model::{ByteRange, LogRow, RowId, SourceId};
use crate::parser::LogParser;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

/// Default capacity for the `mpsc::channel` between a `FileTailer` and a UI
/// consumer. Sized to absorb large bursts without backpressuring the tailer
/// task: sustained throughput is bounded by `capacity ÷ ui_poll_interval`, so
/// a small cap caused the tailer's `send().await` to block long before the
/// engine (which can absorb ~3M rows/s) had any work to do. Rows are batched
/// into [`LogEvent::RowsAppended`] (up to [`MAX_BATCH_ROWS`] each), so each
/// channel slot now carries up to a whole burst rather than a single row.
pub const DEFAULT_TAILER_CHANNEL_CAPACITY: usize = 16384;

/// Maximum rows carried by one [`LogEvent::RowsAppended`] batch. Bounds both
/// the per-event allocation and the latency before a burst becomes visible
/// to the consumer.
const MAX_BATCH_ROWS: usize = 256;

/// Poll fallback interval. File-system notifications (when available) wake
/// the tailer immediately; this tick is the safety net for platforms or
/// filesystems (e.g. NFS) where notifications don't fire, and bounds how
/// long a stop request can go unnoticed.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Identity of a file generation. A change in `(device, inode)` for the
/// watched path means logrotate-style rename rotation: the path now names a
/// brand-new file, regardless of its length. Length-only checks miss the
/// rename case whenever the replacement grows past the old read offset
/// before the next poll.
#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// An open file generation: the buffered reader plus the `(device, inode)`
/// identity captured at open time, used to detect rename rotation.
type OpenGeneration = (BufReader<File>, Option<(u64, u64)>);

fn trim_line_terminator(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

async fn send_or_stop(sender: &mpsc::Sender<LogEvent>, stop: &AtomicBool, event: LogEvent) {
    // Sending into a closed channel means the consumer has gone away (UI
    // exited, runtime tore down). Flip `stop` so the loop winds down instead
    // of spinning forever.
    if sender.send(event).await.is_err() {
        stop.store(true, Ordering::Relaxed);
    }
}

enum ReadOutcome {
    /// No more complete lines available right now.
    Eof,
    /// The batch filled up; more data may be waiting.
    BatchFull,
    Error(String),
}

/// Read complete lines into `batch` until EOF, a full batch, or an error.
///
/// `offset` counts bytes that have been consumed *and emitted*; `pending`
/// holds the bytes of a trailing line whose newline has not arrived yet
/// (a writer flushing mid-line). Holding the partial instead of emitting it
/// avoids the classic half-line-then-second-bogus-row failure. When
/// `flush_partial_at_eof` is set (one-shot reads and end-of-generation
/// drains) the partial is emitted as the final row instead. Lines are
/// converted lossily, so invalid UTF-8 produces replacement characters
/// rather than a permanently erroring source.
#[allow(clippy::too_many_arguments)]
async fn read_batch(
    reader: &mut BufReader<File>,
    offset: &mut u64,
    pending: &mut Vec<u8>,
    next_row: &mut u64,
    source_id: SourceId,
    parser: &dyn LogParser,
    flush_partial_at_eof: bool,
    batch: &mut Vec<LogRow>,
) -> ReadOutcome {
    let mut buf = Vec::new();
    loop {
        if batch.len() >= MAX_BATCH_ROWS {
            return ReadOutcome::BatchFull;
        }
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => {
                if flush_partial_at_eof && !pending.is_empty() {
                    let end = *offset + pending.len() as u64;
                    let text = String::from_utf8_lossy(trim_line_terminator(pending));
                    let row = parser.parse_line(
                        source_id,
                        RowId(*next_row),
                        ByteRange {
                            start: *offset,
                            end,
                        },
                        text.as_ref(),
                    );
                    *next_row += 1;
                    *offset = end;
                    pending.clear();
                    batch.push(row);
                }
                return ReadOutcome::Eof;
            }
            Ok(n) => {
                if buf.last() == Some(&b'\n') {
                    let end = *offset + (pending.len() + n) as u64;
                    let line: &[u8] = if pending.is_empty() {
                        &buf
                    } else {
                        pending.extend_from_slice(&buf);
                        pending
                    };
                    let text = String::from_utf8_lossy(trim_line_terminator(line));
                    let row = parser.parse_line(
                        source_id,
                        RowId(*next_row),
                        ByteRange {
                            start: *offset,
                            end,
                        },
                        text.as_ref(),
                    );
                    *next_row += 1;
                    *offset = end;
                    pending.clear();
                    batch.push(row);
                } else {
                    // EOF mid-line: the bytes are consumed from the reader
                    // (its position is `offset + pending.len()`), but the row
                    // is withheld until its newline arrives.
                    pending.extend_from_slice(&buf);
                    return ReadOutcome::Eof;
                }
            }
            Err(err) => return ReadOutcome::Error(err.to_string()),
        }
    }
}

/// Drain everything currently readable, sending batched rows as they fill.
/// Returns `Err(())` when the handle should be dropped and reopened (a read
/// error was already reported).
#[allow(clippy::too_many_arguments)]
async fn pump_rows(
    reader: &mut BufReader<File>,
    offset: &mut u64,
    pending: &mut Vec<u8>,
    next_row: &mut u64,
    source_id: SourceId,
    parser: &dyn LogParser,
    flush_partial_at_eof: bool,
    sender: &mpsc::Sender<LogEvent>,
    stop: &AtomicBool,
) -> Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut batch = Vec::new();
        let outcome = read_batch(
            reader,
            offset,
            pending,
            next_row,
            source_id,
            parser,
            flush_partial_at_eof,
            &mut batch,
        )
        .await;
        if !batch.is_empty() {
            send_or_stop(sender, stop, LogEvent::RowsAppended(batch)).await;
        }
        match outcome {
            ReadOutcome::BatchFull => continue,
            ReadOutcome::Eof => return Ok(()),
            ReadOutcome::Error(message) => {
                send_or_stop(sender, stop, LogEvent::SourceError { source_id, message }).await;
                return Err(());
            }
        }
    }
}

pub struct FileTailer {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl FileTailer {
    pub fn start(
        source_id: SourceId,
        path: PathBuf,
        parser: Arc<dyn LogParser>,
        sender: mpsc::Sender<LogEvent>,
        from_start: bool,
        follow: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            let stop = stop_clone;
            send_or_stop(
                &sender,
                &stop,
                LogEvent::SourceAdded {
                    source_id,
                    path: path.clone(),
                },
            )
            .await;

            // File-system notification wakeups; the poll tick below remains
            // the fallback. The watcher callback runs on notify's own
            // thread, so a bounded(1) channel coalesces event bursts into a
            // single pending wakeup. The parent directory watch sees rename
            // rotation and re-creation; the file itself is watched too (and
            // re-watched per generation below) because directory-level
            // backends like kqueue don't report appends to contained files.
            let (wake_tx, mut wake_rx) = mpsc::channel::<()>(1);
            let mut watcher = {
                let wake = wake_tx.clone();
                let mut watcher = notify::recommended_watcher(move |_event| {
                    let _ = wake.try_send(());
                })
                .ok();
                if let Some(active) = watcher.as_mut() {
                    let watch_root = match path.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                        _ => PathBuf::from("."),
                    };
                    if active
                        .watch(&watch_root, RecursiveMode::NonRecursive)
                        .is_err()
                    {
                        watcher = None;
                    }
                }
                watcher
            };
            // Keep a sender alive even without a watcher so `wake_rx.recv()`
            // pends instead of resolving `None` in a busy loop.
            let _wake_keepalive = wake_tx;

            let mut offset = 0u64;
            let mut pending: Vec<u8> = Vec::new();
            let mut next_row = 0u64;
            let mut initialized = false;
            let mut current: Option<OpenGeneration> = None;

            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                if current.is_none() {
                    match open_at(&path, &mut offset, &mut initialized, from_start).await {
                        Ok(opened) => {
                            pending.clear();
                            current = Some(opened);
                            // (Re-)watch this generation of the file for
                            // append wakeups; the old generation's watch
                            // died with its directory entry. Failure is
                            // fine — the poll tick still covers us.
                            if let Some(active) = watcher.as_mut() {
                                let _ = active.unwatch(&path);
                                let _ = active.watch(&path, RecursiveMode::NonRecursive);
                            }
                        }
                        Err(message) => {
                            send_or_stop(
                                &sender,
                                &stop,
                                LogEvent::SourceError { source_id, message },
                            )
                            .await;
                        }
                    }
                }

                // Rotation and truncation checks go against the *path*, not
                // the open handle: a rename swaps the file under the path
                // while the handle keeps reading the old generation.
                // A metadata error means the path is briefly missing
                // mid-rotation: keep draining the old handle; the reopen
                // happens once the new file exists and the identity check
                // fires.
                if let Some((reader, identity)) = current.as_mut()
                    && let Ok(meta) = tokio::fs::metadata(&path).await
                {
                    let renamed = matches!(
                        (identity.as_ref(), file_identity(&meta)),
                        (Some(old), Some(new)) if *old != new
                    );
                    if renamed {
                        // Finish the old generation first — it is still open
                        // on this handle and its tail (including any held
                        // partial) is final.
                        let _ = pump_rows(
                            reader,
                            &mut offset,
                            &mut pending,
                            &mut next_row,
                            source_id,
                            parser.as_ref(),
                            true,
                            &sender,
                            &stop,
                        )
                        .await;
                        send_or_stop(&sender, &stop, LogEvent::SourceRotated { source_id }).await;
                        current = None;
                        offset = 0;
                        pending.clear();
                        // Reopen the new generation immediately.
                        continue;
                    }
                    if meta.len() < offset + pending.len() as u64 {
                        // In-place truncation: restart from the top.
                        send_or_stop(&sender, &stop, LogEvent::SourceRotated { source_id }).await;
                        offset = 0;
                        pending.clear();
                        if reader.seek(SeekFrom::Start(0)).await.is_err() {
                            current = None;
                        }
                    }
                }

                if let Some((reader, _)) = current.as_mut()
                    && pump_rows(
                        reader,
                        &mut offset,
                        &mut pending,
                        &mut next_row,
                        source_id,
                        parser.as_ref(),
                        !follow,
                        &sender,
                        &stop,
                    )
                    .await
                    .is_err()
                {
                    current = None;
                }

                if !follow {
                    break;
                }

                tokio::select! {
                    _ = wake_rx.recv() => {
                        // Coalesce any burst of FS events into this wakeup.
                        while wake_rx.try_recv().is_ok() {}
                    }
                    _ = sleep(POLL_INTERVAL) => {}
                }
            }
            send_or_stop(&sender, &stop, LogEvent::SourceRemoved { source_id }).await;
        });

        Self { stop, handle }
    }

    /// Request shutdown without waiting for the spawned task to finish.
    /// Non-async so callers (e.g. UI `Drop` impls) can avoid `block_on`,
    /// which would block the UI thread and panic when invoked from a
    /// Tokio worker context. The task observes the flag on its next
    /// iteration and exits; the runtime drives it to completion when
    /// the runtime itself is dropped.
    pub fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.await;
    }
}

/// Open `path` and seek to the right starting point: the carried `offset`
/// when resuming, byte 0 for `from_start`, or the current end otherwise.
async fn open_at(
    path: &Path,
    offset: &mut u64,
    initialized: &mut bool,
    from_start: bool,
) -> Result<(BufReader<File>, Option<(u64, u64)>), String> {
    let mut file = File::open(path).await.map_err(|err| err.to_string())?;
    let meta = file.metadata().await.map_err(|err| err.to_string())?;
    let len = meta.len();
    let identity = file_identity(&meta);
    let seek_to = if *initialized {
        (*offset).min(len)
    } else if from_start {
        0
    } else {
        len
    };
    file.seek(SeekFrom::Start(seek_to))
        .await
        .map_err(|err| err.to_string())?;
    *offset = seek_to;
    *initialized = true;
    Ok((BufReader::new(file), identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::CompositeParser;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncWriteExt;

    fn drain_rows(rx: &mut mpsc::Receiver<LogEvent>, rows: &mut Vec<LogRow>, rotated: &mut bool) {
        while let Ok(event) = rx.try_recv() {
            match event {
                LogEvent::RowAppended(row) => rows.push(row),
                LogEvent::RowsAppended(batch) => rows.extend(batch),
                LogEvent::SourceRotated { .. } => *rotated = true,
                _ => {}
            }
        }
    }

    /// Poll the receiver until `condition` is satisfied or ~2s elapse.
    async fn wait_for(
        rx: &mut mpsc::Receiver<LogEvent>,
        rows: &mut Vec<LogRow>,
        rotated: &mut bool,
        condition: impl Fn(&[LogRow], bool) -> bool,
    ) -> bool {
        for _ in 0..40 {
            drain_rows(rx, rows, rotated);
            if condition(rows, *rotated) {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn reads_existing_lines_once_when_not_following() {
        let tmp = NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"INFO started\nERROR failed\n")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let tailer = FileTailer::start(
            SourceId(1),
            tmp.path().to_path_buf(),
            Arc::new(CompositeParser::default()),
            tx,
            true,
            false,
        );
        tailer.stop().await;

        let mut rows = Vec::new();
        let mut rotated = false;
        drain_rows(&mut rx, &mut rows, &mut rotated);
        assert!(rows.len() <= 2);
    }

    #[tokio::test]
    async fn follows_appended_lines() {
        let tmp = NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"first\n").await.unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let tailer = FileTailer::start(
            SourceId(2),
            tmp.path().to_path_buf(),
            Arc::new(CompositeParser::default()),
            tx,
            true,
            true,
        );

        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path())
            .await
            .unwrap();
        f.write_all(b"second\n").await.unwrap();
        f.flush().await.unwrap();

        let mut rows = Vec::new();
        let mut rotated = false;
        let seen = wait_for(&mut rx, &mut rows, &mut rotated, |rows, _| {
            rows.iter().any(|row| row.message.as_ref() == "second")
        })
        .await;
        tailer.stop().await;
        assert!(seen);
    }

    #[tokio::test]
    async fn follows_appended_lines_when_starting_at_end() {
        let tmp = NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"first\n").await.unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let tailer = FileTailer::start(
            SourceId(3),
            tmp.path().to_path_buf(),
            Arc::new(CompositeParser::default()),
            tx,
            false,
            true,
        );

        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path())
            .await
            .unwrap();
        f.write_all(b"second\n").await.unwrap();
        f.flush().await.unwrap();

        let mut rows = Vec::new();
        let mut rotated = false;
        let seen = wait_for(&mut rx, &mut rows, &mut rotated, |rows, _| {
            rows.iter().any(|row| row.message.as_ref() == "second")
        })
        .await;
        tailer.stop().await;

        assert!(seen);
        assert!(!rows.iter().any(|row| row.message.as_ref() == "first"));
    }

    #[tokio::test]
    async fn detects_rotation_by_rename_even_when_new_file_is_larger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        tokio::fs::write(&path, b"aa\n").await.unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let tailer = FileTailer::start(
            SourceId(7),
            path.clone(),
            Arc::new(CompositeParser::default()),
            tx,
            true,
            true,
        );

        let mut rows = Vec::new();
        let mut rotated = false;
        assert!(
            wait_for(&mut rx, &mut rows, &mut rotated, |rows, _| {
                rows.iter().any(|row| row.message.as_ref() == "aa")
            })
            .await
        );

        // Rename rotation where the replacement is *larger* than the old
        // read offset — a length-only check cannot detect this.
        tokio::fs::rename(&path, dir.path().join("app.log.1"))
            .await
            .unwrap();
        tokio::fs::write(&path, b"bbbbbbbbbbbb\n").await.unwrap();

        let seen = wait_for(&mut rx, &mut rows, &mut rotated, |rows, rotated| {
            rotated
                && rows
                    .iter()
                    .any(|row| row.message.as_ref() == "bbbbbbbbbbbb")
        })
        .await;
        tailer.stop().await;

        assert!(
            seen,
            "rotation must be detected and the new file read from byte 0"
        );
    }

    #[tokio::test]
    async fn holds_partial_lines_until_newline_arrives() {
        let tmp = NamedTempFile::new().unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let tailer = FileTailer::start(
            SourceId(8),
            tmp.path().to_path_buf(),
            Arc::new(CompositeParser::default()),
            tx,
            true,
            true,
        );

        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path())
            .await
            .unwrap();
        f.write_all(b"par").await.unwrap();
        f.flush().await.unwrap();

        // The unterminated line must be held, not emitted.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut rows = Vec::new();
        let mut rotated = false;
        drain_rows(&mut rx, &mut rows, &mut rotated);
        assert!(rows.is_empty(), "partial line must wait for its newline");

        f.write_all(b"tial\n").await.unwrap();
        f.flush().await.unwrap();

        let seen = wait_for(&mut rx, &mut rows, &mut rotated, |rows, _| {
            rows.iter().any(|row| row.message.as_ref() == "partial")
        })
        .await;
        tailer.stop().await;
        assert!(seen);
    }

    #[tokio::test]
    async fn invalid_utf8_lines_are_lossily_replaced_not_fatal() {
        let tmp = NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"caf\xFF ok\n").await.unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let tailer = FileTailer::start(
            SourceId(9),
            tmp.path().to_path_buf(),
            Arc::new(CompositeParser::default()),
            tx,
            true,
            false,
        );

        // Non-follow mode finishes on its own; collect until the channel
        // closes rather than racing the first read with a stop request.
        let mut rows = Vec::new();
        let mut errors = 0;
        while let Some(event) = rx.recv().await {
            match event {
                LogEvent::RowAppended(row) => rows.push(row),
                LogEvent::RowsAppended(batch) => rows.extend(batch),
                LogEvent::SourceError { .. } => errors += 1,
                _ => {}
            }
        }
        tailer.stop().await;
        assert_eq!(rows.len(), 1, "invalid UTF-8 must not drop the line");
        assert!(rows[0].message.contains("caf"));
        assert!(rows[0].message.contains("ok"));
        assert_eq!(errors, 0, "invalid UTF-8 must not error the source");
    }

    #[tokio::test]
    async fn resets_to_start_and_emits_rotated_when_file_shrinks() {
        let tmp = NamedTempFile::new().unwrap();
        tokio::fs::write(tmp.path(), b"first line\nsecond line\n")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let tailer = FileTailer::start(
            SourceId(4),
            tmp.path().to_path_buf(),
            Arc::new(CompositeParser::default()),
            tx,
            true,
            true,
        );

        // Let the tailer read the two existing lines.
        tokio::time::sleep(Duration::from_millis(250)).await;
        // Truncate to a shorter generation; the next poll sees file_len < offset
        // and should treat it as a rotation (reset cursor to 0).
        tokio::fs::write(tmp.path(), b"rotated\n").await.unwrap();

        tokio::time::sleep(Duration::from_millis(400)).await;
        tailer.stop().await;

        let mut saw_rotated = false;
        let mut saw_rotated_row = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                LogEvent::SourceRotated { .. } => saw_rotated = true,
                LogEvent::RowAppended(row) if row.message.as_ref() == "rotated" => {
                    saw_rotated_row = true;
                }
                LogEvent::RowsAppended(batch)
                    if batch.iter().any(|row| row.message.as_ref() == "rotated") =>
                {
                    saw_rotated_row = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_rotated,
            "expected a SourceRotated event after truncation"
        );
        assert!(
            saw_rotated_row,
            "expected the post-rotation line to be read from offset 0"
        );
    }

    #[tokio::test]
    async fn reports_source_error_for_unreadable_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.log");

        let (tx, mut rx) = mpsc::channel(16);
        // follow = false: the open-failure path runs once and the task ends.
        let tailer = FileTailer::start(
            SourceId(5),
            missing,
            Arc::new(CompositeParser::default()),
            tx,
            false,
            false,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        tailer.stop().await;

        let mut saw_error = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, LogEvent::SourceError { .. }) {
                saw_error = true;
            }
        }
        assert!(saw_error, "expected a SourceError for a non-existent path");
    }
}
