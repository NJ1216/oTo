use super::{DynamicLimiter, DynamicPermit, CANCELLED_ERROR};
use crate::JobCancellation;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use tokio::task::{Id, JoinSet};

/// Bound running and completed-but-uncollected workers across all batches.
/// Reap while waiting for input or admission, so neither wait strands permits.
pub(super) async fn run_bounded<Item, Meta, Output, Task>(
    mut input: Receiver<Item>,
    admission: &Arc<DynamicLimiter>,
    cancellation: &JobCancellation,
    mut prepare: impl FnMut(Item) -> (Meta, Task),
    mut finished: impl FnMut(Meta, Result<Output, String>),
    input_closed: impl FnOnce(),
) where
    Output: Send + 'static,
    Task: Future<Output = Output> + Send + 'static,
{
    let mut workers = JoinSet::new();
    let mut identities: HashMap<Id, (Meta, DynamicPermit)> = HashMap::new();
    let mut pending = None;
    let mut input_closed = Some(input_closed);
    loop {
        if input_closed.is_none() && pending.is_none() && workers.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            joined = workers.join_next_with_id(), if !workers.is_empty() => {
                let (id, result) = match joined.expect("worker set is not empty") {
                    Ok((id, result)) => (id, Ok(result)),
                    Err(error) => {
                        let message = if error.is_cancelled() {
                            CANCELLED_ERROR.to_owned()
                        } else {
                            format!("conversion worker failed: {error}")
                        };
                        (error.id(), Err(message))
                    }
                };
                let (metadata, _permit) = identities.remove(&id).expect("worker identity exists");
                finished(metadata, result);
            }
            permit = admission.acquire(cancellation), if pending.is_some() => {
                let (metadata, task) = prepare(pending.take().expect("pending input exists"));
                match permit {
                    Ok(permit) => {
                        let handle = workers.spawn(task);
                        // Retain admission until collection, including on worker panic.
                        identities.insert(handle.id(), (metadata, permit));
                    }
                    Err(()) => finished(metadata, Err(CANCELLED_ERROR.to_owned())),
                }
            }
            item = input.recv(), if input_closed.is_some() && pending.is_none() => {
                pending = item;
                if pending.is_none() {
                    input_closed.take().expect("input closes once")();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, Semaphore};

    #[tokio::test]
    async fn large_input_stream_keeps_only_a_bounded_number_of_worker_futures() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct LiveFuture(Arc<AtomicUsize>);
        impl Drop for LiveFuture {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let count = 10_000;
        let admission = Arc::new(DynamicLimiter::new(5));
        let (send, receive) = mpsc::channel(4);
        let producer = tokio::spawn(async move {
            for id in 0..count {
                send.send(id).await.unwrap();
            }
        });
        let live = Arc::new(AtomicUsize::new(0));
        let mut peak = 0;
        let mut completed = 0;
        let mut sum = 0;
        run_bounded(
            receive,
            &admission,
            &JobCancellation::new(),
            |id| {
                peak = peak.max(live.fetch_add(1, Ordering::SeqCst) + 1);
                let lifetime = LiveFuture(live.clone());
                (id, async move {
                    tokio::task::yield_now().await;
                    drop(lifetime);
                    id
                })
            },
            |id, result| {
                assert_eq!(result.unwrap(), id);
                completed += 1;
                sum += id;
            },
            || {},
        )
        .await;
        producer.await.unwrap();
        assert_eq!(completed, count);
        assert_eq!(sum, count * (count - 1) / 2);
        assert!(peak <= 5, "unbounded worker allocation: {peak}");
        assert_eq!(live.load(Ordering::SeqCst), 0);
        eprintln!("bounded scheduler: {completed} inputs, peak {peak} live worker futures");
    }

    #[tokio::test]
    async fn worker_panic_retains_identity_and_does_not_abort_other_results() {
        let (send, receive) = mpsc::channel(4);
        for id in 0..3 {
            send.send(id).await.unwrap();
        }
        drop(send);
        let admission = Arc::new(DynamicLimiter::new(2));
        let cancellation = JobCancellation::new();
        let mut results = Vec::new();
        let mut closed = false;
        run_bounded(
            receive,
            &admission,
            &cancellation,
            |id| {
                (id, async move {
                    assert_ne!(id, 0, "injected worker failure");
                    id * 10
                })
            },
            |id, result| results.push((id, result)),
            || closed = true,
        )
        .await;
        results.sort_by_key(|(id, _)| *id);
        assert_eq!(results.len(), 3);
        assert!(results[0]
            .1
            .as_ref()
            .unwrap_err()
            .contains("injected worker failure"));
        assert_eq!(results[1], (1, Ok(10)));
        assert_eq!(results[2], (2, Ok(20)));
        assert!(closed);
        assert_eq!(
            admission.running.load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }

    #[tokio::test]
    async fn shared_admission_bounds_batches_and_accepts_parallelism_changes() {
        let admission = Arc::new(DynamicLimiter::new(1));
        let gate = Arc::new(Semaphore::new(0));
        let (started, mut starts) = mpsc::unbounded_channel();
        let mut batches = JoinSet::new();
        for batch in 0..2 {
            let (send, receive) = mpsc::channel(4);
            for id in 0..3 {
                send.send(batch * 10 + id).await.unwrap();
            }
            drop(send);
            let admission = admission.clone();
            let gate = gate.clone();
            let started = started.clone();
            batches.spawn(async move {
                let mut results = Vec::new();
                run_bounded(
                    receive,
                    &admission,
                    &JobCancellation::new(),
                    |id| {
                        let gate = gate.clone();
                        let started = started.clone();
                        (id, async move {
                            started.send(id).unwrap();
                            gate.acquire().await.unwrap().forget();
                            id
                        })
                    },
                    |id, result| results.push((id, result.unwrap())),
                    || {},
                )
                .await;
                results
            });
        }
        starts.recv().await.unwrap();
        tokio::task::yield_now().await;
        assert!(starts.try_recv().is_err());
        admission.set_limit(2);
        starts.recv().await.unwrap();
        tokio::task::yield_now().await;
        assert!(starts.try_recv().is_err());
        assert_eq!(
            admission.running.load(std::sync::atomic::Ordering::Acquire),
            2
        );
        admission.set_limit(1);
        gate.add_permits(1);
        tokio::task::yield_now().await;
        assert!(
            starts.try_recv().is_err(),
            "lowering the limit must wait for drainage"
        );
        gate.add_permits(5);
        let mut results = Vec::new();
        while let Some(batch) = batches.join_next().await {
            results.extend(batch.unwrap());
        }
        results.sort();
        assert_eq!(
            results,
            [(0, 0), (1, 1), (2, 2), (10, 10), (11, 11), (12, 12)]
        );
        assert_eq!(
            admission.running.load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }

    #[tokio::test]
    async fn cancellation_drains_admission_waiters_without_starting_workers() {
        let (send, receive) = mpsc::channel(4);
        for id in 0..3 {
            send.send(id).await.unwrap();
        }
        drop(send);
        let admission = Arc::new(DynamicLimiter::new(1));
        let cancellation = JobCancellation::new();
        let _occupied = admission.acquire(&cancellation).await.unwrap();
        cancellation.cancel();
        let mut results = Vec::new();
        run_bounded(
            receive,
            &admission,
            &cancellation,
            |id| {
                (id, async {
                    panic!("cancelled worker must never be polled")
                })
            },
            |id, result: Result<(), String>| results.push((id, result.unwrap_err())),
            || {},
        )
        .await;
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|(_, error)| error == CANCELLED_ERROR));
    }

    #[tokio::test]
    async fn queue_closes_before_running_worker_finishes() {
        let (send, receive) = mpsc::channel(1);
        send.send(()).await.unwrap();
        drop(send);
        let admission = Arc::new(DynamicLimiter::new(1));
        let (closed, closing) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut gate = Some(gate);
            run_bounded(
                receive,
                &admission,
                &JobCancellation::new(),
                |_| {
                    let gate = gate.take().unwrap();
                    ((), async move {
                        gate.await.unwrap();
                    })
                },
                |_, result| result.unwrap(),
                || closed.send(()).unwrap(),
            )
            .await;
        });
        closing.await.unwrap();
        assert!(
            !task.is_finished(),
            "exact progress begins while the last worker is running"
        );
        release.send(()).unwrap();
        task.await.unwrap();
    }
}
