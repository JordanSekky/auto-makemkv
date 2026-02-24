use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;

use tokio::io::{AsyncRead, AsyncWrite};

pub trait AsyncReadWithSize: AsyncRead {
    fn total_read(&self) -> Arc<AtomicUsize>;
    fn total_size(&self) -> Arc<AtomicUsize>;

    fn progress(&self) -> f64 {
        let read = self.total_read().load(Ordering::Relaxed);
        let size = self.total_size().load(Ordering::Relaxed);
        if size == 0 {
            0.0
        } else {
            read as f64 / size as f64
        }
    }
}

pub struct AsyncReadWithSizeImpl<R: AsyncRead> {
    inner: R,
    total_read: Arc<AtomicUsize>,
    total_size: Arc<AtomicUsize>,
}

impl<R: AsyncRead> AsyncReadWithSizeImpl<R> {
    pub fn new(inner: R, total_size: usize) -> Self {
        Self {
            inner,
            total_read: Arc::new(AtomicUsize::new(0)),
            total_size: Arc::new(AtomicUsize::new(total_size)),
        }
    }

    pub fn inner(&self) -> &R {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.inner
    }
}

impl<R: AsyncRead> AsyncReadWithSize for AsyncReadWithSizeImpl<R> {
    fn total_read(&self) -> Arc<AtomicUsize> {
        self.total_read.clone()
    }

    fn total_size(&self) -> Arc<AtomicUsize> {
        self.total_size.clone()
    }
}

impl<R: AsyncRead> AsyncRead for AsyncReadWithSizeImpl<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // SAFETY: `inner` is structurally pinned; we never move it out of `self`.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        let start_filled = buf.filled().len();
        let result = inner.poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let diff_filled = buf.filled().len() - start_filled;
            this.total_read.fetch_add(diff_filled, Ordering::Relaxed);
        }
        result
    }
}

pub struct AsyncWriteWithSizeImpl<W: AsyncWrite> {
    inner: W,
    total_written: usize,
    total_size: usize,
}

impl<W: AsyncWrite> AsyncWriteWithSizeImpl<W> {
    pub fn new(inner: W, total_size: usize) -> Self {
        Self {
            inner,
            total_written: 0,
            total_size,
        }
    }
}

pub trait AsyncWriteWithSize: AsyncWrite {
    fn total_written(&self) -> usize;
    fn total_size(&self) -> usize;

    fn progress(&self) -> f64 {
        self.total_written() as f64 / self.total_size() as f64
    }
}

impl<W: AsyncWrite> AsyncWriteWithSize for AsyncWriteWithSizeImpl<W> {
    fn total_written(&self) -> usize {
        self.total_written
    }

    fn total_size(&self) -> usize {
        self.total_size
    }
}

impl<W: AsyncWrite> AsyncWrite for AsyncWriteWithSizeImpl<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // SAFETY: `inner` is structurally pinned; we never move it out of `self`.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        let result = inner.poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            this.total_written += n;
        }
        result
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        // SAFETY: `inner` is structurally pinned; we never move it out of `self`.
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        // SAFETY: `inner` is structurally pinned; we never move it out of `self`.
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_shutdown(cx)
    }
}
