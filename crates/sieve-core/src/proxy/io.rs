//! Stream adapters for CONNECT tunnels.

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Replays bytes that were already read (to sniff the protocol) before
/// reading from the stream again.
pub(crate) struct Rewind<T> {
    prefix: Bytes,
    inner: T,
}

impl<T> Rewind<T> {
    pub(crate) fn new(prefix: Bytes, inner: T) -> Self {
        Self { prefix, inner }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Rewind<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Rewind<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Counts the bytes read from and written to a stream, so a tunnel can report
/// its size even when it ends with an error.
pub(crate) struct Counted<T> {
    inner: T,
    pub(crate) read: Arc<AtomicU64>,
    pub(crate) written: Arc<AtomicU64>,
}

impl<T> Counted<T> {
    pub(crate) fn new(inner: T) -> Self {
        Self {
            inner,
            read: Arc::default(),
            written: Arc::default(),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            self.written.fetch_add(n as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn rewind_replays_the_prefix_first() {
        let mut io = Rewind::new(Bytes::from_static(b"GE"), &b"T /"[..]);
        let mut out = String::new();
        io.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "GET /");
    }

    #[tokio::test]
    async fn counted_tracks_both_directions() {
        let (a, mut b) = tokio::io::duplex(64);
        let mut a = Counted::new(a);
        a.write_all(b"hello").await.unwrap();
        b.write_all(b"hi").await.unwrap();
        let mut buf = [0; 2];
        a.read_exact(&mut buf).await.unwrap();
        assert_eq!(a.written.load(Ordering::Relaxed), 5);
        assert_eq!(a.read.load(Ordering::Relaxed), 2);
    }
}
