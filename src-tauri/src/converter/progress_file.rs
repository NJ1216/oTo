use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct ProgressSnapshot {
    pub out_time_us: u64,
    pub ended: bool,
}

/// Tail FFmpeg's append-only progress file without rereading previous polls.
/// Incomplete lines survive EOF; only newline-terminated records are applied.
#[derive(Default)]
pub(super) struct ProgressFileReader {
    offset: u64,
    partial: Vec<u8>,
    snapshot: ProgressSnapshot,
}

impl ProgressFileReader {
    pub async fn poll(&mut self, path: &Path) -> std::io::Result<ProgressSnapshot> {
        let mut file = tokio::fs::File::open(path).await?;
        let length = file.metadata().await?.len();
        if length < self.offset {
            // A rewritten file must not join an old partial line to new data or
            // regress the displayed timestamp.
            self.offset = 0;
            self.partial.clear();
            self.snapshot.ended = false;
        }
        file.seek(std::io::SeekFrom::Start(self.offset)).await?;
        let mut buffer = [0_u8; 8192];
        while self.offset < length {
            let limit = (length - self.offset).min(buffer.len() as u64) as usize;
            let read = file.read(&mut buffer[..limit]).await?;
            if read == 0 {
                break;
            }
            self.offset += read as u64;
            self.consume(&buffer[..read]);
        }
        Ok(self.snapshot)
    }

    fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                let line = self.partial.trim_ascii();
                if let Some(value) = line.strip_prefix(b"out_time_us=") {
                    if let Some(time) = std::str::from_utf8(value)
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        self.snapshot.out_time_us = self.snapshot.out_time_us.max(time);
                    }
                } else if line == b"progress=end" {
                    self.snapshot.ended = true;
                }
                self.partial.clear();
            } else {
                self.partial.push(byte);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn fragmented_records_preserve_timestamps_and_do_not_finish_early() {
        let mut reader = ProgressFileReader::default();
        for byte in b"out_time_us=500000\r\nprogress=continue\n" {
            reader.consume(&[*byte]);
        }
        assert_eq!(reader.snapshot.out_time_us, 500_000);
        reader.consume(b"out_time_us=N/A\nout_time_us=-1\nout_time_us=100\ninvalid=\xff\n");
        assert_eq!(reader.snapshot.out_time_us, 500_000);
        reader.consume(b"out_time_us=1500");
        assert_eq!(reader.snapshot.out_time_us, 500_000);
        reader.consume(b"000\nprogress=en");
        assert_eq!(reader.snapshot.out_time_us, 1_500_000);
        assert!(!reader.snapshot.ended);
        reader.consume(b"d\n");
        assert!(reader.snapshot.ended);
    }

    #[tokio::test]
    async fn polling_reads_only_appended_bytes_and_recovers_from_truncation() {
        let root = std::env::temp_dir().join(format!("oto-tail-test-{}", uuid::Uuid::new_v4()));
        let manager = crate::spool::SpoolManager::initialize(&root).unwrap();
        let progress = manager.new_temporary_file("progress", "txt");
        let mut reader = ProgressFileReader::default();
        assert!(reader.poll(progress.path()).await.is_err());
        std::fs::write(progress.path(), b"out_time_us=100000\npartial").unwrap();
        assert_eq!(
            reader.poll(progress.path()).await.unwrap().out_time_us,
            100_000
        );
        // Overwrite the consumed prefix with a larger timestamp. A full-file
        // reread would incorrectly advance the next snapshot to 900000.
        std::fs::OpenOptions::new()
            .write(true)
            .open(progress.path())
            .unwrap()
            .write_all(b"out_time_us=900000\n")
            .unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(progress.path())
            .unwrap();
        writer
            .write_all(b" line\nout_time_us=200000\nprogress=en")
            .unwrap();
        assert_eq!(
            reader.poll(progress.path()).await.unwrap(),
            ProgressSnapshot {
                out_time_us: 200_000,
                ended: false,
            }
        );
        writer.write_all(b"d\n").unwrap();
        assert!(reader.poll(progress.path()).await.unwrap().ended);
        let offset = reader.offset;
        reader.poll(progress.path()).await.unwrap();
        assert_eq!(reader.offset, offset);
        drop(writer);
        std::fs::write(progress.path(), b"out_time_us=1\n").unwrap();
        assert_eq!(
            reader.poll(progress.path()).await.unwrap(),
            ProgressSnapshot {
                out_time_us: 200_000,
                ended: false,
            }
        );
        drop(progress);
        manager.cleanup_current_session().unwrap();
        drop(manager);
        std::fs::remove_dir_all(root).unwrap();
    }
}
