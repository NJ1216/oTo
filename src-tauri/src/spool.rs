use base64::Engine;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const LEGACY_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum JournalRecord {
    UploadPending { id: String, path: String },
    UploadDone { id: String },
}

struct RecoverySession {
    metadata_dir: PathBuf,
    spool_dir: PathBuf,
    journal_path: PathBuf,
    lock: Option<File>,
}

struct PendingLocalDelete {
    path: PathBuf,
    bytes: usize,
    usage: Arc<AtomicUsize>,
}

/// Owns one process' local spool namespace and the durable cleanup journals.
/// The lock files remain open for the lifetime of this value.
pub struct SpoolManager {
    session_id: String,
    spool_dir: PathBuf,
    metadata_dir: PathBuf,
    _lock: File,
    journal: Mutex<File>,
    recovery_sessions: Mutex<Vec<RecoverySession>>,
    pending_local_deletes: Mutex<Vec<PendingLocalDelete>>,
}

impl SpoolManager {
    pub fn initialize(app_data_dir: &Path) -> std::io::Result<Arc<Self>> {
        let metadata_root = app_data_dir.join("spool-sessions");
        let spool_root = std::env::temp_dir().join("oto-spool");
        std::fs::create_dir_all(&metadata_root)?;
        std::fs::create_dir_all(&spool_root)?;

        let mut recovery_sessions = Vec::new();
        for entry in std::fs::read_dir(&metadata_root)?.flatten() {
            let metadata_dir = entry.path();
            if !metadata_dir.is_dir() {
                continue;
            }
            if metadata_dir.join("cleanup.complete").exists() {
                let _ = std::fs::remove_dir_all(&metadata_dir);
                continue;
            }
            let lock_path = metadata_dir.join("session.lock");
            let Ok(lock) = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
            else {
                continue;
            };
            if lock.try_lock_exclusive().is_err() {
                // A live oTo process owns this session.
                continue;
            }
            let Some(id) = metadata_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            recovery_sessions.push(RecoverySession {
                metadata_dir: metadata_dir.clone(),
                journal_path: metadata_dir.join("recovery.jsonl"),
                spool_dir: spool_root.join(id),
                lock: Some(lock),
            });
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        let metadata_dir = metadata_root.join(&session_id);
        let spool_dir = spool_root.join(&session_id);
        std::fs::create_dir_all(&metadata_dir)?;
        std::fs::create_dir_all(&spool_dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(metadata_dir.join("session.lock"))?;
        lock.lock_exclusive()?;
        let mut identity = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(metadata_dir.join("instance.uuid"))?;
        identity.write_all(session_id.as_bytes())?;
        identity.sync_data()?;
        let journal = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(metadata_dir.join("recovery.jsonl"))?;

        let manager = Arc::new(Self {
            session_id,
            spool_dir,
            metadata_dir,
            _lock: lock,
            journal: Mutex::new(journal),
            recovery_sessions: Mutex::new(recovery_sessions),
            pending_local_deletes: Mutex::new(Vec::new()),
        });
        manager.retry_recovery();
        cleanup_legacy_spools(&spool_root);
        Ok(manager)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn new_spool_file(
        self: &Arc<Self>,
        kind: &str,
        extension: &str,
        usage: Arc<AtomicUsize>,
    ) -> LocalSpoolFile {
        let safe_extension = extension.trim_start_matches('.');
        let filename = if safe_extension.is_empty() {
            format!("{kind}-{}", uuid::Uuid::new_v4())
        } else {
            format!("{kind}-{}.{}", uuid::Uuid::new_v4(), safe_extension)
        };
        LocalSpoolFile {
            path: self.spool_dir.join(filename),
            bytes: 0,
            usage,
            manager: self.clone(),
        }
    }

    pub fn begin_upload(self: &Arc<Self>, path: &Path) -> std::io::Result<UploadGuard> {
        let id = uuid::Uuid::new_v4().to_string();
        self.append_current(&JournalRecord::UploadPending {
            id: id.clone(),
            path: encode_path(path),
        })?;
        Ok(UploadGuard {
            id,
            path: path.to_path_buf(),
            manager: self.clone(),
            completed: false,
        })
    }

    fn append_current(&self, record: &JournalRecord) -> std::io::Result<()> {
        let mut journal = self.journal.lock().unwrap();
        append_record(&mut journal, record)
    }

    fn queue_failed_local_delete(&self, path: PathBuf, bytes: usize, usage: Arc<AtomicUsize>) {
        self.pending_local_deletes
            .lock()
            .unwrap()
            .push(PendingLocalDelete { path, bytes, usage });
    }

    /// Retry local deletions and every crash journal whose lock was acquirable.
    /// Failed NAS removals remain journalled for the next job or process launch.
    pub fn retry_recovery(&self) {
        {
            let mut pending = self.pending_local_deletes.lock().unwrap();
            pending.retain(|item| match remove_file_if_present(&item.path) {
                Ok(()) => {
                    release_usage(&item.usage, item.bytes);
                    false
                }
                Err(error) => {
                    eprintln!("failed to remove spool {}: {error}", item.path.display());
                    true
                }
            });
        }

        let current_path = self.metadata_dir.join("recovery.jsonl");
        {
            // Append and compaction must not race: otherwise a newly appended pending
            // upload could be lost when a completed journal is truncated.
            let _journal = self.journal.lock().unwrap();
            replay_upload_journal(&current_path);
        }

        let mut sessions = self.recovery_sessions.lock().unwrap();
        let mut remaining = Vec::new();
        for mut session in std::mem::take(&mut *sessions) {
            let local_clean = match std::fs::remove_dir_all(&session.spool_dir) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => {
                    eprintln!(
                        "failed to recover spool directory {}: {error}",
                        session.spool_dir.display()
                    );
                    false
                }
            };
            let uploads_clean = replay_upload_journal(&session.journal_path);
            if local_clean && uploads_clean {
                let cleanup_marker = session.metadata_dir.join("cleanup.complete");
                let marker_result = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&cleanup_marker)
                    .and_then(|mut marker| {
                        marker.write_all(b"complete")?;
                        marker.sync_data()
                    });
                if marker_result.is_err() {
                    let _ = std::fs::remove_file(&cleanup_marker);
                    remaining.push(session);
                    continue;
                }
                // Windows cannot remove an open lock file, so release it before deleting
                // the complete metadata namespace. The marker prevents another instance
                // from adopting the directory in the short interval after unlock.
                drop(session.lock.take());
                match std::fs::remove_dir_all(&session.metadata_dir) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => eprintln!(
                        "failed to remove recovered session metadata {}: {error}",
                        session.metadata_dir.display()
                    ),
                }
            } else {
                remaining.push(session);
            }
        }
        *sessions = remaining;
    }

    /// Called only after all conversion tasks have been joined.
    pub fn cleanup_current_session(&self) -> std::io::Result<()> {
        self.retry_recovery();
        match std::fs::remove_dir(&self.spool_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

pub struct LocalSpoolFile {
    path: PathBuf,
    bytes: usize,
    usage: Arc<AtomicUsize>,
    manager: Arc<SpoolManager>,
}

impl LocalSpoolFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn add_accounted_bytes(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.usage.fetch_add(bytes, Ordering::AcqRel);
    }

    pub fn set_accounted_bytes(&mut self, bytes: usize) {
        if bytes > self.bytes {
            self.usage.fetch_add(bytes - self.bytes, Ordering::AcqRel);
        } else {
            release_usage(&self.usage, self.bytes - bytes);
        }
        self.bytes = bytes;
    }
}

impl Drop for LocalSpoolFile {
    fn drop(&mut self) {
        match remove_file_if_present(&self.path) {
            Ok(()) => release_usage(&self.usage, self.bytes),
            Err(error) => {
                eprintln!("deferred spool removal {}: {error}", self.path.display());
                self.manager.queue_failed_local_delete(
                    self.path.clone(),
                    self.bytes,
                    self.usage.clone(),
                );
            }
        }
    }
}

pub struct UploadGuard {
    id: String,
    path: PathBuf,
    manager: Arc<SpoolManager>,
    completed: bool,
}

impl UploadGuard {
    pub fn complete(mut self) -> std::io::Result<()> {
        self.manager.append_current(&JournalRecord::UploadDone {
            id: self.id.clone(),
        })?;
        self.completed = true;
        Ok(())
    }
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if remove_file_if_present(&self.path).is_ok()
            && self
                .manager
                .append_current(&JournalRecord::UploadDone {
                    id: self.id.clone(),
                })
                .is_ok()
        {
            self.completed = true;
        }
    }
}

fn append_record(file: &mut File, record: &JournalRecord) -> std::io::Result<()> {
    serde_json::to_writer(&mut *file, record)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn replay_upload_journal(path: &Path) -> bool {
    let mut pending = HashMap::<String, PathBuf>::new();
    if let Ok(file) = File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(record) = serde_json::from_str::<JournalRecord>(&line) else {
                continue;
            };
            match record {
                JournalRecord::UploadPending { id, path } => {
                    if let Some(path) = decode_path(&path) {
                        pending.insert(id, path);
                    }
                }
                JournalRecord::UploadDone { id } => {
                    pending.remove(&id);
                }
            }
        }
    }
    if pending.is_empty() {
        return compact_completed_journal(path).is_ok();
    }
    let Ok(mut journal) = OpenOptions::new().append(true).create(true).open(path) else {
        return false;
    };
    for (id, upload) in pending.clone() {
        match remove_file_if_present(&upload) {
            Ok(()) => {
                if append_record(&mut journal, &JournalRecord::UploadDone { id: id.clone() })
                    .is_ok()
                {
                    pending.remove(&id);
                }
            }
            Err(error) => eprintln!("deferred upload cleanup {}: {error}", upload.display()),
        }
    }
    pending.is_empty() && compact_completed_journal(path).is_ok()
}

fn compact_completed_journal(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.sync_data()
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn release_usage(usage: &AtomicUsize, bytes: usize) {
    let _ = usage.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(bytes))
    });
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    base64::engine::general_purpose::STANDARD.encode(path.as_os_str().as_bytes())
}

#[cfg(unix)]
fn decode_path(encoded: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    Some(std::ffi::OsString::from_vec(bytes).into())
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(windows)]
fn decode_path(encoded: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(std::ffi::OsString::from_wide(&wide).into())
}

fn cleanup_legacy_spools(new_spool_root: &Path) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        if entry.path() == new_spool_root {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("oto-input-") || name.starts_with("oto-output-")) {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= LEGACY_GRACE_PERIOD);
        if old_enough {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_round_trips_through_journal_encoding() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', 0xff,
        ]));
        assert_eq!(decode_path(&encode_path(&path)), Some(path));
    }

    #[test]
    fn unicode_path_round_trips_through_journal_encoding() {
        let path = PathBuf::from("/tmp/音声/途中ファイル.m4a");
        assert_eq!(decode_path(&encode_path(&path)), Some(path));
    }

    #[test]
    fn normal_local_spool_completion_removes_file_and_session_directory() {
        let app_data =
            std::env::temp_dir().join(format!("oto-normal-spool-test-{}", uuid::Uuid::new_v4()));
        let manager = SpoolManager::initialize(&app_data).unwrap();
        let usage = Arc::new(AtomicUsize::new(0));
        let mut spool = manager.new_spool_file("input", "tmp", usage.clone());
        std::fs::write(spool.path(), b"complete").unwrap();
        spool.set_accounted_bytes(8);
        let spool_path = spool.path().to_path_buf();

        drop(spool);
        manager.cleanup_current_session().unwrap();

        assert!(!spool_path.exists());
        assert!(!manager.spool_dir.exists());
        assert_eq!(usage.load(Ordering::Acquire), 0);
        drop(manager);
        let _ = std::fs::remove_dir_all(app_data);
    }

    #[test]
    fn not_found_upload_is_completed_during_replay() {
        let root = std::env::temp_dir().join(format!("oto-journal-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let journal_path = root.join("recovery.jsonl");
        let missing = root.join("missing-upload");
        let mut journal = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&journal_path)
            .unwrap();
        append_record(
            &mut journal,
            &JournalRecord::UploadPending {
                id: "one".into(),
                path: encode_path(&missing),
            },
        )
        .unwrap();
        assert!(replay_upload_journal(&journal_path));
        assert!(std::fs::read_to_string(&journal_path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn active_instance_spool_is_skipped_then_recovered_after_unlock() {
        let app_data =
            std::env::temp_dir().join(format!("oto-lock-recovery-test-{}", uuid::Uuid::new_v4()));
        let active = SpoolManager::initialize(&app_data).unwrap();
        let active_path = active.spool_dir.join("input-crash-test.dat");
        std::fs::write(&active_path, b"still active").unwrap();

        let other = SpoolManager::initialize(&app_data).unwrap();
        assert!(
            active_path.exists(),
            "a locked session must not be recovered"
        );
        drop(other);
        drop(active); // Releases the session lock.

        let recovering = SpoolManager::initialize(&app_data).unwrap();
        assert!(
            !active_path.exists(),
            "an unlocked crashed session is recovered"
        );
        drop(recovering);
        let _ = std::fs::remove_dir_all(app_data);
    }

    #[test]
    fn completed_upload_journal_is_compacted_to_empty() {
        let app_data =
            std::env::temp_dir().join(format!("oto-journal-compact-test-{}", uuid::Uuid::new_v4()));
        let manager = SpoolManager::initialize(&app_data).unwrap();
        let uploaded = app_data.join("uploaded.tmp");
        std::fs::write(&uploaded, b"complete").unwrap();
        manager.begin_upload(&uploaded).unwrap().complete().unwrap();

        manager.retry_recovery();

        let journal = manager.metadata_dir.join("recovery.jsonl");
        assert!(std::fs::read_to_string(journal).unwrap().is_empty());
        std::fs::remove_file(uploaded).unwrap();
        drop(manager);
        let _ = std::fs::remove_dir_all(app_data);
    }

    #[test]
    fn recovered_session_metadata_is_removed_with_its_spool() {
        let app_data = std::env::temp_dir().join(format!(
            "oto-metadata-recovery-test-{}",
            uuid::Uuid::new_v4()
        ));
        let crashed = SpoolManager::initialize(&app_data).unwrap();
        let crashed_metadata = crashed.metadata_dir.clone();
        let crashed_spool = crashed.spool_dir.clone();
        std::fs::write(crashed_spool.join("leftover.tmp"), b"leftover").unwrap();
        drop(crashed);

        let recovering = SpoolManager::initialize(&app_data).unwrap();

        assert!(!crashed_spool.exists());
        assert!(!crashed_metadata.exists());
        drop(recovering);
        let _ = std::fs::remove_dir_all(app_data);
    }

    #[test]
    fn inaccessible_upload_record_is_retained_until_a_later_retry() {
        let root = std::env::temp_dir().join(format!("oto-retry-test-{}", uuid::Uuid::new_v4()));
        let blocked_path = root.join("currently-a-directory");
        std::fs::create_dir_all(&blocked_path).unwrap();
        let journal_path = root.join("recovery.jsonl");
        let mut journal = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&journal_path)
            .unwrap();
        append_record(
            &mut journal,
            &JournalRecord::UploadPending {
                id: "retry".into(),
                path: encode_path(&blocked_path),
            },
        )
        .unwrap();

        assert!(!replay_upload_journal(&journal_path));
        std::fs::remove_dir(&blocked_path).unwrap();
        assert!(replay_upload_journal(&journal_path));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn usage_is_not_released_until_failed_local_delete_succeeds() {
        let app_data =
            std::env::temp_dir().join(format!("oto-accounting-test-{}", uuid::Uuid::new_v4()));
        let manager = SpoolManager::initialize(&app_data).unwrap();
        let usage = Arc::new(AtomicUsize::new(0));
        let mut spool = manager.new_spool_file("output", "tmp", usage.clone());
        std::fs::create_dir(spool.path()).unwrap();
        spool.set_accounted_bytes(123);
        let path = spool.path().to_path_buf();
        drop(spool);
        assert_eq!(usage.load(Ordering::Acquire), 123);

        std::fs::remove_dir(path).unwrap();
        manager.retry_recovery();
        assert_eq!(usage.load(Ordering::Acquire), 0);
        let _ = manager.cleanup_current_session();
        drop(manager);
        let _ = std::fs::remove_dir_all(app_data);
    }
}
