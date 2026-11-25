use crate::spool::LocalSpoolFile;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct State {
    files: HashMap<String, LocalSpoolFile>,
    closed: bool,
    generation: u64,
}

/// Keep decoded audio alive until the frontend releases it or the app exits.
/// Only exact paths issued by this instance can be released.
#[derive(Default)]
pub struct PreviewFiles(Mutex<State>);

impl PreviewFiles {
    pub fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    pub fn retain(&self, file: LocalSpoolFile, generation: u64) -> Result<String, String> {
        let path = file.path().to_string_lossy().into_owned();
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err("application is exiting".into());
        }
        if state.generation != generation {
            return Err("preview window was closed".into());
        }
        state.files.insert(path.clone(), file);
        Ok(path)
    }

    pub fn release(&self, path: &str) -> Result<(), String> {
        let file = self.0.lock().unwrap().files.remove(path);
        match file {
            Some(file) => {
                drop(file); // Failed deletions remain owned by SpoolManager for retry.
                Ok(())
            }
            None => Err("invalid preview path".into()),
        }
    }

    pub fn close(&self) {
        let files = {
            let mut state = self.0.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.files)
        };
        drop(files);
    }

    pub fn invalidate(&self) {
        let files = {
            let mut state = self.0.lock().unwrap();
            state.generation = state.generation.wrapping_add(1);
            std::mem::take(&mut state.files)
        };
        drop(files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::SpoolManager;

    #[test]
    fn preview_release_is_scoped_to_issued_paths_and_shutdown_rejects_late_results() {
        let app_data =
            std::env::temp_dir().join(format!("oto-preview-test-{}", uuid::Uuid::new_v4()));
        let manager = SpoolManager::initialize(&app_data).unwrap();
        let other_manager = SpoolManager::initialize(&app_data).unwrap();
        let previews = PreviewFiles::default();
        let generation = previews.generation();
        let other = other_manager.new_temporary_file("preview", "wav");
        std::fs::write(other.path(), b"other process").unwrap();
        assert!(previews.release(&other.path().to_string_lossy()).is_err());

        let file = manager.new_temporary_file("preview", "wav");
        std::fs::write(file.path(), b"preview").unwrap();
        let path = previews.retain(file, generation).unwrap();
        manager.retry_recovery();
        other_manager.retry_recovery();
        assert!(std::path::Path::new(&path).exists());
        assert!(previews.release(&format!("{path}/../preview.wav")).is_err());
        previews.release(&path).unwrap();
        assert!(!std::path::Path::new(&path).exists());
        assert!(other.path().exists());

        let held = manager.new_temporary_file("preview", "wav");
        std::fs::write(held.path(), b"held").unwrap();
        let held_path = previews.retain(held, generation).unwrap();
        let late = manager.new_temporary_file("preview", "wav");
        let late_path = late.path().to_path_buf();
        std::fs::write(&late_path, b"finished during shutdown").unwrap();
        previews.close();
        assert!(!std::path::Path::new(&held_path).exists());
        assert!(previews.retain(late, generation).is_err());
        assert!(!late_path.exists());
        drop(other);
        manager.cleanup_current_session().unwrap();
        other_manager.cleanup_current_session().unwrap();
        drop((manager, other_manager));
        std::fs::remove_dir_all(app_data).unwrap();
    }

    #[test]
    fn closing_window_releases_retained_files_and_rejects_late_decode_after_reopen() {
        let app_data =
            std::env::temp_dir().join(format!("oto-preview-close-test-{}", uuid::Uuid::new_v4()));
        let manager = SpoolManager::initialize(&app_data).unwrap();
        let previews = PreviewFiles::default();
        let old_generation = previews.generation();
        let held = manager.new_temporary_file("preview", "wav");
        std::fs::write(held.path(), b"held").unwrap();
        let held_path = previews.retain(held, old_generation).unwrap();
        let late = manager.new_temporary_file("preview", "wav");
        let late_path = late.path().to_path_buf();
        std::fs::write(&late_path, b"late").unwrap();
        previews.invalidate();
        assert!(!std::path::Path::new(&held_path).exists());
        let new = manager.new_temporary_file("preview", "wav");
        std::fs::write(new.path(), b"new window").unwrap();
        let new_path = previews.retain(new, previews.generation()).unwrap();
        assert!(previews.retain(late, old_generation).is_err());
        assert!(!late_path.exists());
        assert!(std::path::Path::new(&new_path).exists());
        previews.release(&new_path).unwrap();
        manager.cleanup_current_session().unwrap();
        drop(manager);
        std::fs::remove_dir_all(app_data).unwrap();
    }
}
