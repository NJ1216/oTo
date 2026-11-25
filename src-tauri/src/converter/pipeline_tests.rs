use super::*;
use std::sync::Mutex;

/// Exercise the actual scheduler, CPU permit, spool guards and upload copier.
/// A gate stands in for a slow NAS; destinations are disposable local files.
async fn exercise_upload_overlap(cancel_uploads: bool) {
    let root = std::env::temp_dir().join(format!("oto-pipeline-test-{}", uuid::Uuid::new_v4()));
    let manager = crate::spool::SpoolManager::initialize(&root).unwrap();
    let resources = Arc::new(ConversionResources::new(1));
    let cancellation = Arc::new(crate::JobCancellation::new());
    let usage = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let progress = Arc::new(Mutex::new(HashMap::<usize, Vec<f64>>::new()));
    let (converted, mut conversions) = tokio::sync::mpsc::unbounded_channel();
    let (uploading, mut uploads) = tokio::sync::mpsc::unbounded_channel();
    let (send, receive) = tokio::sync::mpsc::channel(4);
    for id in 0..4 {
        send.send(id).await.unwrap();
    }
    drop(send);
    let job = tokio::spawn({
        let manager = manager.clone();
        let resources = resources.clone();
        let cancellation = cancellation.clone();
        let usage = usage.clone();
        let gate = gate.clone();
        let progress = progress.clone();
        let root = root.clone();
        async move {
            let mut results = Vec::new();
            worker_queue::run_bounded(
                receive,
                &resources.workers,
                &cancellation,
                |id| {
                    let manager = manager.clone();
                    let resources = resources.clone();
                    let cancellation = cancellation.clone();
                    let usage = usage.clone();
                    let gate = gate.clone();
                    let progress = progress.clone();
                    let converted = converted.clone();
                    let uploading = uploading.clone();
                    let destination = root.join(format!("output-{id}.wav"));
                    (id, async move {
                        let bytes = 64;
                        resources
                            .output_spool
                            .reserved
                            .fetch_add(bytes, Ordering::AcqRel);
                        let _reservation = OutputReservation {
                            bytes,
                            capacity: resources.output_spool.clone(),
                        };
                        let mut spool = manager.new_spool_file("output", "wav", usage);
                        let cpu = resources
                            .cpu
                            .acquire(&cancellation)
                            .await
                            .map_err(|_| CANCELLED_ERROR.to_string())?;
                        cpu.run(async {
                            std::fs::write(spool.path(), [id as u8; 64]).unwrap();
                            spool.set_accounted_bytes(bytes as usize);
                        })
                        .await;
                        progress
                            .lock()
                            .unwrap()
                            .entry(id)
                            .or_default()
                            .push(ffmpeg_stage_progress(1.0));
                        converted.send((id, spool.path().to_path_buf())).unwrap();
                        let _upload_permit = tokio::select! {
                            permit = resources.network_output.acquire() => permit.unwrap(),
                            _ = cancellation.cancelled() => return Err(CANCELLED_ERROR.to_string()),
                        };
                        uploading.send(id).unwrap();
                        tokio::select! {
                            permit = gate.acquire() => permit.unwrap().forget(),
                            _ = cancellation.cancelled() => return Err(CANCELLED_ERROR.to_string()),
                        }
                        copy_spool_to_network(
                            spool.path(),
                            &destination,
                            &manager,
                            &cancellation,
                            Arc::new(move |ratio| {
                                progress
                                    .lock()
                                    .unwrap()
                                    .entry(id)
                                    .or_default()
                                    .push(output_stage_progress(ratio));
                            }),
                        )
                        .await
                        .map_err(|error| error.to_string())
                    })
                },
                |id, result| results.push((id, result.and_then(|result| result))),
                || {},
            )
            .await;
            results
        }
    });

    let mut pending_paths = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for _ in 0..3 {
            pending_paths.push(conversions.recv().await.unwrap().1);
        }
        uploads.recv().await.unwrap();
    })
    .await
    .expect("later conversions should finish while the first upload is blocked");
    assert!(
        conversions.try_recv().is_err(),
        "fourth artifact must wait for bounded admission"
    );
    assert!(
        uploads.try_recv().is_err(),
        "network uploads must stay serialized"
    );
    assert_eq!(resources.cpu.running.load(Ordering::Acquire), 0);
    assert_eq!(resources.workers.running.load(Ordering::Acquire), 3);
    assert_eq!(
        resources.output_spool.reserved.load(Ordering::Acquire),
        3 * 64
    );
    assert_eq!(usage.load(Ordering::Acquire), 3 * 64);
    assert!(pending_paths.iter().all(|path| path.exists()));
    assert!(progress
        .lock()
        .unwrap()
        .values()
        .all(|values| values == &[0.9]));

    if cancel_uploads {
        cancellation.cancel();
    } else {
        gate.add_permits(4);
    }
    let results = tokio::time::timeout(std::time::Duration::from_secs(5), job)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(results.len(), 4);
    for (id, result) in results {
        let destination = root.join(format!("output-{id}.wav"));
        if cancel_uploads {
            assert!(result.unwrap_err().contains(CANCELLED_ERROR));
            assert!(!destination.exists());
        } else {
            result.unwrap();
            assert_eq!(std::fs::read(destination).unwrap(), vec![id as u8; 64]);
            let progress = progress.lock().unwrap();
            let values = &progress[&id];
            assert_eq!(values.last(), Some(&1.0));
            assert!(values.windows(2).all(|p| p[0] <= p[1]));
        }
    }
    assert_eq!(resources.workers.running.load(Ordering::Acquire), 0);
    assert_eq!(resources.cpu.running.load(Ordering::Acquire), 0);
    assert_eq!(resources.network_output.available_permits(), 1);
    assert_eq!(resources.output_spool.reserved.load(Ordering::Acquire), 0);
    assert_eq!(usage.load(Ordering::Acquire), 0);
    assert!(pending_paths.iter().all(|path| !path.exists()));
    manager.cleanup_current_session().unwrap();
    drop(manager);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cpu_work_overlaps_upload_but_spools_and_reservations_survive_until_copy_finishes() {
    exercise_upload_overlap(false).await;
}

#[tokio::test]
async fn cancelling_an_upload_backlog_releases_spools_reservations_and_waiting_workers() {
    exercise_upload_overlap(true).await;
}

#[tokio::test]
async fn cpu_permit_is_released_when_work_fails_or_its_future_is_aborted() {
    let limiter = Arc::new(DynamicLimiter::new(1));
    let cancellation = crate::JobCancellation::new();
    let permit = limiter.acquire(&cancellation).await.unwrap();
    assert_eq!(
        permit
            .run(async { Err::<(), _>("failed conversion") })
            .await,
        Err("failed conversion")
    );
    assert_eq!(limiter.running.load(Ordering::Acquire), 0);

    let permit = limiter.acquire(&cancellation).await.unwrap();
    let (started, running) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(permit.run(async move {
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    }));
    running.await.unwrap();
    assert_eq!(limiter.running.load(Ordering::Acquire), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(limiter.running.load(Ordering::Acquire), 0);
}
