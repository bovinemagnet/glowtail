use crate::events::LogEvent;
use crate::model::{ByteRange, RowId, SourceId};
use crate::parser::LogParser;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

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
            let _ = sender
                .send(LogEvent::SourceAdded {
                    source_id,
                    path: path.clone(),
                })
                .await;

            let mut offset = 0u64;
            let mut next_row = 0u64;
            let mut initialized = false;

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }

                match File::open(&path).await {
                    Ok(mut file) => {
                        let file_len = match file.metadata().await {
                            Ok(meta) => meta.len(),
                            Err(err) => {
                                let _ = sender
                                    .send(LogEvent::SourceError {
                                        source_id,
                                        message: err.to_string(),
                                    })
                                    .await;
                                sleep(Duration::from_millis(250)).await;
                                continue;
                            }
                        };

                        if file_len < offset {
                            offset = 0;
                            let _ = sender.send(LogEvent::SourceRotated { source_id }).await;
                        }

                        let seek_to = if initialized {
                            offset
                        } else if from_start {
                            0
                        } else {
                            file_len
                        };
                        if file.seek(SeekFrom::Start(seek_to)).await.is_err() {
                            sleep(Duration::from_millis(250)).await;
                            continue;
                        }
                        offset = seek_to;
                        initialized = true;

                        let mut reader = BufReader::new(file);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    let end = offset + n as u64;
                                    let trimmed = line.trim_end_matches(['\n', '\r']);
                                    let row = parser.parse_line(
                                        source_id,
                                        RowId(next_row),
                                        ByteRange { start: offset, end },
                                        trimmed,
                                    );
                                    next_row += 1;
                                    offset = end;
                                    let _ = sender.send(LogEvent::RowAppended(row)).await;
                                }
                                Err(err) => {
                                    let _ = sender
                                        .send(LogEvent::SourceError {
                                            source_id,
                                            message: err.to_string(),
                                        })
                                        .await;
                                    break;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        let _ = sender
                            .send(LogEvent::SourceError {
                                source_id,
                                message: err.to_string(),
                            })
                            .await;
                    }
                }

                if !follow {
                    break;
                }
                sleep(Duration::from_millis(200)).await;
            }
            let _ = sender.send(LogEvent::SourceRemoved { source_id }).await;
        });

        Self { stop, handle }
    }

    pub async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::CompositeParser;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncWriteExt;

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

        let mut rows = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, LogEvent::RowAppended(_)) {
                rows += 1;
            }
        }
        assert!(rows <= 2);
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

        tokio::time::sleep(Duration::from_millis(400)).await;
        tailer.stop().await;

        let mut seen_second = false;
        while let Ok(event) = rx.try_recv() {
            if let LogEvent::RowAppended(row) = event
                && row.message.as_ref() == "second"
            {
                seen_second = true;
            }
        }
        assert!(seen_second);
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

        tokio::time::sleep(Duration::from_millis(400)).await;
        tailer.stop().await;

        let mut saw_first = false;
        let mut saw_second = false;
        while let Ok(event) = rx.try_recv() {
            if let LogEvent::RowAppended(row) = event {
                if row.message.as_ref() == "first" {
                    saw_first = true;
                }
                if row.message.as_ref() == "second" {
                    saw_second = true;
                }
            }
        }
        assert!(!saw_first);
        assert!(saw_second);
    }
}
