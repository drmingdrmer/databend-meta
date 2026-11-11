// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use databend_meta_runtime_api::BoxFuture;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::sync::watch;
use tonic::transport::server::Connected;

/// Keep graceful draining, but fail the actual transport after its shutdown budget.
///
/// In particular, HTTP/2 graceful shutdown does not interrupt an unfinished
/// handshake. Timing out the listener alone would leave Tonic's detached
/// connection tasks holding their sockets and service references.
pub(super) struct ShutdownIo<T> {
    inner: T,
    deadline: BoxFuture<'static, ()>,
    expired: bool,
}

impl<T> ShutdownIo<T> {
    pub(super) fn new(inner: T, mut shutdown: watch::Receiver<()>, grace: Duration) -> Self {
        let deadline = Box::pin(async move {
            let _ = shutdown.changed().await;
            tokio::time::sleep(grace).await;
        });
        Self {
            inner,
            deadline,
            expired: false,
        }
    }

    fn poll_deadline(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.expired && self.deadline.as_mut().poll(cx).is_ready() {
            self.expired = true;
            log::warn!("raft connection exceeded its shutdown grace period");
        }
        if self.expired {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "raft connection shutdown deadline",
            ))
        } else {
            Ok(())
        }
    }
}

impl<T: Connected> Connected for ShutdownIo<T> {
    type ConnectInfo = T::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ShutdownIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_deadline(cx)?;
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ShutdownIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.poll_deadline(cx)?;
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.poll_deadline(cx)?;
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_deadline(cx)?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_deadline(cx)?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::watch;

    use super::ShutdownIo;

    #[tokio::test]
    async fn pending_read_expires_without_peer_activity() {
        let (transport, _peer) = tokio::io::duplex(1);
        let (tx, rx) = watch::channel(());
        let mut io = ShutdownIo::new(transport, rx, Duration::from_millis(20));
        tx.send(()).unwrap();
        let err = tokio::time::timeout(Duration::from_secs(1), io.read_u8())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // A completed deadline remains an error rather than polling its future again.
        assert_eq!(
            io.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn pending_write_expires_without_peer_activity() {
        let (transport, _peer) = tokio::io::duplex(1);
        let (tx, rx) = watch::channel(());
        let mut io = ShutdownIo::new(transport, rx, Duration::from_millis(20));
        io.write_all(b"x").await.unwrap();
        tx.send(()).unwrap();
        let err = tokio::time::timeout(Duration::from_secs(1), io.write_all(b"y"))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn idle_connections_have_no_deadline_before_shutdown() {
        let (transport, mut peer) = tokio::io::duplex(8);
        let (_tx, rx) = watch::channel(());
        let mut io = ShutdownIo::new(transport, rx, Duration::from_millis(1));
        let reader = tokio::spawn(async move { io.read_u8().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        peer.write_all(b"a").await.unwrap();
        assert_eq!(reader.await.unwrap().unwrap(), b'a');
    }

    #[tokio::test]
    async fn traffic_can_finish_during_grace() {
        let (transport, mut peer) = tokio::io::duplex(8);
        let (tx, rx) = watch::channel(());
        let mut io = ShutdownIo::new(transport, rx, Duration::from_secs(5));
        tx.send(()).unwrap();
        peer.write_all(b"a").await.unwrap();
        assert_eq!(io.read_u8().await.unwrap(), b'a');
        io.write_all(b"b").await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(peer.read_u8().await.unwrap(), b'b');
        io.shutdown().await.unwrap();
        assert_eq!(
            peer.read_u8().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
