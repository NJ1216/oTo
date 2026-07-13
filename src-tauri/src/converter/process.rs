use std::sync::Arc;

pub type PidCallback = Arc<dyn Fn(u32) + Send + Sync>;

#[derive(Clone)]
pub struct ProcessTracker {
    on_start: PidCallback,
    on_exit: PidCallback,
}

impl ProcessTracker {
    pub fn new(on_start: PidCallback, on_exit: PidCallback) -> Self {
        Self { on_start, on_exit }
    }

    pub fn register(&self, pid: Option<u32>) -> Option<ProcessRegistration> {
        pid.map(|pid| {
            (self.on_start)(pid);
            ProcessRegistration {
                pid: Some(pid),
                on_exit: self.on_exit.clone(),
            }
        })
    }
}

pub struct ProcessRegistration {
    pid: Option<u32>,
    on_exit: PidCallback,
}

impl Drop for ProcessRegistration {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            (self.on_exit)(pid);
        }
    }
}

/// Keep FFmpeg children invisible on Windows and independently signalable on Unix.
pub fn configure_ffmpeg_command(command: &mut tokio::process::Command) {
    command.kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn registration_tracks_start_and_unregisters_on_drop() {
        let starts = Arc::new(AtomicUsize::new(0));
        let exits = Arc::new(AtomicUsize::new(0));
        let tracker = ProcessTracker::new(
            {
                let starts = starts.clone();
                Arc::new(move |_| {
                    starts.fetch_add(1, Ordering::SeqCst);
                })
            },
            {
                let exits = exits.clone();
                Arc::new(move |_| {
                    exits.fetch_add(1, Ordering::SeqCst);
                })
            },
        );

        let registration = tracker.register(Some(42));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(exits.load(Ordering::SeqCst), 0);
        drop(registration);
        assert_eq!(exits.load(Ordering::SeqCst), 1);
    }
}
