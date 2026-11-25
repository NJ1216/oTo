use crate::JobCancellation;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
}

/// Own preview work independently of an IPC caller's lifetime. Shutdown cancels
/// and joins it before removing spool directories, including waveform reads.
#[derive(Default)]
pub struct PreviewOperations {
    state: Mutex<State>,
    cancellation: Arc<JobCancellation>,
    /// Token of the newest `run_latest` operation; a newer one cancels it.
    latest: Mutex<Option<Arc<JobCancellation>>>,
    finished: tokio::sync::Notify,
}

struct Operation(Arc<PreviewOperations>);

impl Drop for Operation {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().active -= 1;
        self.0.finished.notify_waiters();
    }
}

impl PreviewOperations {
    pub async fn run<T, F>(
        self: &Arc<Self>,
        work: impl FnOnce(Arc<JobCancellation>) -> F + Send + 'static,
    ) -> Result<T, String>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, String>> + Send + 'static,
    {
        let operation = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err("application is exiting".into());
            }
            state.active += 1;
            Operation(self.clone())
        };
        let cancellation = self.cancellation.clone();
        tokio::spawn(async move {
            let _operation = operation;
            work(cancellation).await
        })
        .await
        .map_err(|error| format!("preview task failed: {error}"))?
    }

    /// Like `run`, but a newer `run_latest` call cancels this one. The work gets
    /// its own token, which also fires when the application exits.
    pub async fn run_latest<T, F>(
        self: &Arc<Self>,
        work: impl FnOnce(Arc<JobCancellation>) -> F + Send + 'static,
    ) -> Result<T, String>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, String>> + Send + 'static,
    {
        let own = Arc::new(JobCancellation::new());
        if let Some(previous) = self.latest.lock().unwrap().replace(own.clone()) {
            previous.cancel();
        }
        let result = self
            .run({
                let own = own.clone();
                move |exit| async move {
                    let bridge = tokio::spawn({
                        let own = own.clone();
                        async move {
                            exit.cancelled().await;
                            own.cancel();
                        }
                    });
                    let result = work(own).await;
                    bridge.abort();
                    result
                }
            })
            .await;
        let mut latest = self.latest.lock().unwrap();
        if latest
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &own))
        {
            *latest = None;
        }
        result
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cancellation.cancel();
    }

    pub async fn wait_finished(&self) {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().unwrap().active == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub async fn run_process(
    mut command: tokio::process::Command,
    cancellation: &JobCancellation,
) -> Result<(), String> {
    if cancellation.is_cancelled() {
        return Err("application is exiting".into());
    }
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let child = command.spawn().map_err(|error| error.to_string())?;
    wait_process(child, cancellation).await
}

async fn wait_process(
    mut child: tokio::process::Child,
    cancellation: &JobCancellation,
) -> Result<(), String> {
    let stderr = child.stderr.take();
    let reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(mut stderr) = stderr {
            stderr.read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let status = tokio::select! {
        status = child.wait() => status.map_err(|error| error.to_string()),
        _ = cancellation.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err("application is exiting".into())
        }
    };
    let stderr = reader
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let status = status?;
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr);
        return Err(format!(
            "ffmpeg failed ({status}): {}",
            text.lines().last().unwrap_or("unknown error")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_newer_latest_operation_cancels_the_earlier_one() {
        let operations = Arc::new(PreviewOperations::default());
        let (started, ready) = tokio::sync::oneshot::channel();
        let first = tokio::spawn({
            let operations = operations.clone();
            async move {
                operations
                    .run_latest(move |cancel| async move {
                        started.send(()).unwrap();
                        cancel.cancelled().await;
                        Err::<(), _>("cancelled".to_string())
                    })
                    .await
            }
        });
        ready.await.unwrap();
        let second = operations.run_latest(|cancel| async move {
            assert!(!cancel.is_cancelled());
            Ok(7)
        });
        assert_eq!(second.await, Ok(7));
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, Err("cancelled".to_string()));
    }

    #[tokio::test]
    async fn exit_cancels_a_latest_operation() {
        let operations = Arc::new(PreviewOperations::default());
        let (started, ready) = tokio::sync::oneshot::channel();
        let running = tokio::spawn({
            let operations = operations.clone();
            async move {
                operations
                    .run_latest(move |cancel| async move {
                        started.send(()).unwrap();
                        cancel.cancelled().await;
                        Ok(())
                    })
                    .await
            }
        });
        ready.await.unwrap();
        operations.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        operations.wait_finished().await;
    }

    #[tokio::test]
    async fn shutdown_rejects_new_work_and_drains_even_if_the_ipc_caller_is_dropped() {
        let operations = Arc::new(PreviewOperations::default());
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn({
            let operations = operations.clone();
            async move {
                operations
                    .run(move |cancel| async move {
                        started.send(()).unwrap();
                        cancel.cancelled().await;
                        blocked.await.unwrap(); // Simulate a final file read/cleanup.
                        Ok(())
                    })
                    .await
            }
        });
        ready.await.unwrap();
        caller.abort();
        let _ = caller.await;
        operations.close();
        assert!(operations
            .run::<(), _>(|_| async { panic!("must not start") })
            .await
            .is_err());
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            operations.wait_finished()
        )
        .await
        .is_err());
        release.send(()).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            operations.wait_finished(),
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reaps_running_child_before_releasing_temporary_file() {
        let operations = Arc::new(PreviewOperations::default());
        let app_data =
            std::env::temp_dir().join(format!("oto-preview-exit-test-{}", uuid::Uuid::new_v4()));
        let manager = crate::spool::SpoolManager::initialize(&app_data).unwrap();
        let file = manager.new_temporary_file("preview", "wav");
        let path = file.path().to_path_buf();
        std::fs::write(&path, b"partial").unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn({
            let operations = operations.clone();
            async move {
                operations
                    .run(move |cancel| async move {
                        let _file = file;
                        let child = tokio::process::Command::new("/bin/sleep")
                            .arg("30")
                            .kill_on_drop(true)
                            .spawn()
                            .unwrap();
                        let pid = child.id().unwrap();
                        started.send(pid).unwrap();
                        wait_process(child, &cancel).await
                    })
                    .await
            }
        });
        let pid = ready.await.unwrap();
        operations.close();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            operations.wait_finished(),
        )
        .await
        .unwrap();
        assert!(caller.await.unwrap().is_err());
        assert!(!path.exists());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        manager.cleanup_current_session().unwrap();
        drop(manager);
        std::fs::remove_dir_all(app_data).unwrap();
    }
}
