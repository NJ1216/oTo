use crate::JobCancellation;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Cancellation is observed at safe boundaries. Always drain buffered writes
/// before the caller releases its file owner, including on read/copy failure.
pub(super) async fn copy_cancellable(
    source: &mut (impl AsyncRead + Unpin),
    target: &mut (impl AsyncWrite + Unpin),
    cancellation: &JobCancellation,
    mut on_chunk: impl FnMut(usize),
) -> io::Result<()> {
    let result: io::Result<()> = async {
        let mut chunk = vec![0_u8; super::NETWORK_CHUNK_SIZE];
        loop {
            check_cancelled(cancellation)?;
            let count = source.read(&mut chunk).await?;
            check_cancelled(cancellation)?;
            if count == 0 {
                return Ok(());
            }
            target.write_all(&chunk[..count]).await?;
            on_chunk(count);
        }
    }
    .await;
    let flushed = target.flush().await;
    result?;
    flushed?;
    check_cancelled(cancellation)
}

fn check_cancelled(cancellation: &JobCancellation) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            super::CANCELLED_ERROR,
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::sync::oneshot;

    struct GatedWriter {
        bytes: Vec<u8>,
        flushing: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
        drained: bool,
    }

    impl AsyncWrite for GatedWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.bytes.extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if let Some(started) = self.flushing.take() {
                started.send(()).unwrap();
            }
            use std::future::Future;
            match Pin::new(&mut self.release).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => {
                    self.drained = true;
                    Poll::Ready(Ok(()))
                }
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn cancellation_during_final_flush_waits_for_write_drain_and_returns_error() {
        let cancellation = Arc::new(JobCancellation::new());
        let (flushing, started) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                let mut target = GatedWriter {
                    bytes: Vec::new(),
                    flushing: Some(flushing),
                    release: blocked,
                    drained: false,
                };
                let result = copy_cancellable(
                    &mut b"audio data".as_slice(),
                    &mut target,
                    &cancellation,
                    |_| {},
                )
                .await;
                (result, target)
            }
        });
        started.await.unwrap();
        cancellation.cancel();
        assert!(
            !task.is_finished(),
            "pending writes must remain owned until drained"
        );
        release.send(()).unwrap();
        let (result, writer) = task.await.unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(writer.drained);
        assert_eq!(writer.bytes, b"audio data");
    }

    #[tokio::test]
    async fn cancellation_after_a_chunk_still_flushes_before_returning() {
        let cancellation = JobCancellation::new();
        let (flushing, started) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        release.send(()).unwrap();
        let mut target = GatedWriter {
            bytes: Vec::new(),
            flushing: Some(flushing),
            release: blocked,
            drained: false,
        };
        let data = vec![1; super::super::NETWORK_CHUNK_SIZE + 1];
        let result = copy_cancellable(&mut data.as_slice(), &mut target, &cancellation, |_| {
            cancellation.cancel()
        })
        .await;
        assert!(result.is_err());
        started.await.unwrap();
        assert!(target.drained);
        assert_eq!(target.bytes.len(), super::super::NETWORK_CHUNK_SIZE);
    }
}
