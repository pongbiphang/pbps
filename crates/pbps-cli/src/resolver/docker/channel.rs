//! A pinned Docker attach stream. This authenticates the daemon hop only;
//! run ownership, the private forwarder and its backend need admission too.

use super::{API, Error, LocalApi, REQUEST_BUDGET, RequestGuard};
use crate::resolver::native::DaemonLease;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Source-free errors; Docker stderr is never a database result or diagnostic.
/// The stream has no reconnect path and becomes unusable after framing or
/// peer-identity failure. It does not grant a resolver admission capability.
pub struct AttachStream {
    io: FramedIo<TokioIo<hyper::upgrade::Upgraded>>,
    daemon: Option<DaemonLease>,
}

impl LocalApi {
    /// Consumes this direct native-daemon connection. The caller must still
    /// establish ownership and isolation of the exact immutable container ID.
    pub async fn attach(self, container_id: &str) -> Result<AttachStream, Error> {
        if self.native_daemon.is_none() {
            return Err(Error::NativeDaemon);
        }
        self.attach_inner(container_id).await
    }

    pub(crate) async fn attach_inner(mut self, container_id: &str) -> Result<AttachStream, Error> {
        if container_id.len() != 64
            || !container_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Profile);
        }
        let mut guard = RequestGuard {
            api: &mut self,
            completed: false,
        };
        let exchange = async {
            if let Some(lease) = &guard.api.native_daemon {
                lease.check().map_err(|_| Error::NativeDaemon)?;
            }
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "{API}/containers/{container_id}/attach?stream=1&stdin=1&stdout=1&stderr=1"
                ))
                .header("Host", "docker")
                .header("Connection", "Upgrade")
                .header("Upgrade", "tcp")
                .body(Full::new(Bytes::new()))
                .map_err(|_| Error::Response)?;
            let reply = guard
                .api
                .sender
                .as_mut()
                .ok_or(Error::ControlLost)?
                .send_request(request)
                .await
                .map_err(|_| Error::ControlLost)?;
            if reply.status() != StatusCode::SWITCHING_PROTOCOLS
                || reply
                    .headers()
                    .get("upgrade")
                    .is_none_or(|v| !v.as_bytes().eq_ignore_ascii_case(b"tcp"))
            {
                return Err(Error::Response);
            }
            let upgraded = hyper::upgrade::on(reply)
                .await
                .map_err(|_| Error::ControlLost)?;
            if let Some(lease) = &guard.api.native_daemon {
                lease.check().map_err(|_| Error::NativeDaemon)?;
            }
            Ok(upgraded)
        };
        let upgraded = tokio::time::timeout(REQUEST_BUDGET, exchange)
            .await
            .map_err(|_| Error::ControlLost)??;
        guard.completed = true;
        drop(guard);
        Ok(AttachStream {
            io: FramedIo::new(TokioIo::new(upgraded)),
            daemon: self.native_daemon.take(),
        })
    }
}

impl AttachStream {
    fn check(&mut self) -> io::Result<()> {
        if self
            .daemon
            .as_ref()
            .is_some_and(|lease| lease.check().is_err())
        {
            self.io.failed = true;
        }
        self.io.check()
    }
}

impl AsyncRead for AttachStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.check()?;
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl AsyncWrite for AttachStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check()?;
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check()?;
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check()?;
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

// Docker non-TTY output is an eight-byte header followed by stdout/stderr.
// Parse incrementally into the caller's bounded buffer; never allocate the
// server's length or mix stderr with authenticated database protocol bytes.
struct FramedIo<S> {
    inner: S,
    header: [u8; 8],
    filled: usize,
    remaining: usize,
    failed: bool,
    read_left: usize,
    write_left: usize,
}

fn invalid_stream() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "resolver private control stream is invalid",
    )
}

impl<S> FramedIo<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            header: [0; 8],
            filled: 0,
            remaining: 0,
            failed: false,
            read_left: 256 * 1024 * 1024,
            write_left: 256 * 1024 * 1024,
        }
    }
    fn check(&self) -> io::Result<()> {
        if self.failed {
            Err(invalid_stream())
        } else {
            Ok(())
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for FramedIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.check()?;
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.read_left == 0 {
                this.failed = true;
                return Poll::Ready(Err(invalid_stream()));
            }
            if this.remaining > 0 {
                let mut storage = [0; 8192];
                let capacity = storage
                    .len()
                    .min(output.remaining())
                    .min(this.remaining)
                    .min(this.read_left);
                let mut chunk = ReadBuf::new(&mut storage[..capacity]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut chunk) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) if !chunk.filled().is_empty() => {
                        let count = chunk.filled().len();
                        this.remaining -= count;
                        this.read_left -= count;
                        output.put_slice(chunk.filled());
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(_) => {
                        this.failed = true;
                        return Poll::Ready(Err(invalid_stream()));
                    }
                }
            }
            let end = (this.filled + this.read_left).min(8);
            let mut header = ReadBuf::new(&mut this.header[this.filled..end]);
            match Pin::new(&mut this.inner).poll_read(cx, &mut header) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) if !header.filled().is_empty() => {
                    this.filled += header.filled().len();
                    this.read_left -= header.filled().len();
                }
                Poll::Ready(Ok(())) if this.filled == 0 => {
                    this.failed = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(_) => {
                    this.failed = true;
                    return Poll::Ready(Err(invalid_stream()));
                }
            }
            if this.filled == 8 {
                let length = u32::from_be_bytes(this.header[4..8].try_into().expect("fixed header"))
                    as usize;
                if this.header[..4] != [1, 0, 0, 0] || length == 0 || length > 1024 * 1024 {
                    this.failed = true;
                    return Poll::Ready(Err(invalid_stream()));
                }
                this.remaining = length;
                this.filled = 0;
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FramedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check()?;
        if self.write_left == 0 && !bytes.is_empty() {
            self.failed = true;
            return Poll::Ready(Err(invalid_stream()));
        }
        let count = self.write_left.min(bytes.len());
        let result = Pin::new(&mut self.inner).poll_write(cx, &bytes[..count]);
        if let Poll::Ready(Ok(count)) = result {
            self.write_left -= count;
        }
        if matches!(result, Poll::Ready(Err(_))) {
            self.failed = true;
            return Poll::Ready(Err(invalid_stream()));
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check()?;
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Err(_))) {
            self.failed = true;
            return Poll::Ready(Err(invalid_stream()));
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        if result.is_ready() {
            self.failed = true;
        }
        result
    }
}

#[cfg(test)]
mod tests;
