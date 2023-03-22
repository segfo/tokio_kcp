use std::{
    fmt::{self, Debug},
    io::{self, ErrorKind},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{future, ready};
use kcp::{Error as KcpError, KcpResult};
use log::trace;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UdpSocket,
};

use crate::{config::KcpConfig, session::KcpSession, skcp::KcpSocket};

#[derive(Debug)]
pub struct Receiver {
    session: Arc<KcpSession>,
    buffer: Vec<u8>,
    buffer_pos: usize,
    buffer_cap: usize,
}
impl Receiver {
    fn new(session: Arc<KcpSession>) -> Self {
        Receiver {
            session,
            buffer: Vec::new(),
            buffer_pos: 0,
            buffer_cap: 0,
        }
    }

    /// `recv` data into `buf`
    pub fn poll_recv(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<KcpResult<usize>> {
        loop {
            // Consumes all data in buffer
            {
                let recv_buffer_pos = self.buffer_pos;
                let recv_buffer_cap = self.buffer_cap;
                if recv_buffer_pos < recv_buffer_cap {
                    let remaining = recv_buffer_cap - recv_buffer_pos;
                    let copy_length = remaining.min(buf.len());

                    buf[..copy_length].copy_from_slice(&self.buffer[recv_buffer_pos..recv_buffer_pos + copy_length]);
                    self.buffer_pos += copy_length;
                    return Ok(copy_length).into();
                }
            }

            // Mutex doesn't have poll_lock, spinning on it.
            let mut kcp = self.session.kcp_socket().lock();

            // Try to read from KCP
            // 1. Read directly with user provided `buf`
            let peek_size = kcp.peek_size().unwrap_or(0);

            // 1.1. User's provided buffer is larger than available buffer's size
            if peek_size > 0 && peek_size <= buf.len() {
                match ready!(kcp.poll_recv(cx, buf)) {
                    Ok(n) => {
                        trace!("[CLIENT] recv directly {} bytes", n);
                        return Ok(n).into();
                    }
                    Err(KcpError::UserBufTooSmall) => {}
                    Err(err) => return Err(err).into(),
                }
            }

            // 2. User `buf` too small, read to recv_buffer
            let required_size = peek_size;
            {
                if self.buffer.len() < required_size {
                    self.buffer.resize(required_size, 0);
                }
                match ready!(kcp.poll_recv(cx, &mut self.buffer)) {
                    Ok(0) => return Ok(0).into(),
                    Ok(n) => {
                        trace!("[CLIENT] recv buffered {} bytes", n);
                        self.buffer_pos = 0;
                        self.buffer_cap = n;
                    }
                    Err(err) => return Err(err).into(),
                }
            }
        }
    }

    /// `recv` data into `buf`
    pub async fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_recv(cx, buf)).await
    }
}
macro_rules! async_run {
    ($block:expr) => {{
        futures::executor::block_on($block)
    }};
}
pub struct Transmitter {
    session: Arc<KcpSession>,
}
impl Transmitter {
    fn new(session: Arc<KcpSession>) -> Self {
        Transmitter { session }
    }

    /// `send` data in `buf`
    pub fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let mut kcp = self.session.kcp_socket().lock();
        let result = ready!(kcp.poll_send(cx, buf));
        self.session.notify();
        result.into()
    }

    /// `send` data in `buf`
    pub async fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        future::poll_fn(|cx| self.poll_send(cx, buf)).await
    }
}

pub struct OwnedWriteHalf {
    inner: Arc<Mutex<Transmitter>>,
}
impl OwnedWriteHalf {
    pub fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        async_run!(async { self.inner.lock().await.poll_send(cx, buf) })
    }
    pub async fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        self.inner.lock().await.send(buf).await
    }
}
#[derive(Debug)]
pub struct OwnedReadHalf {
    inner: Arc<Mutex<Receiver>>,
}
impl OwnedReadHalf {
    pub fn poll_recv(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<KcpResult<usize>> {
        async_run!(async { self.inner.lock().await.poll_recv(cx, buf) })
    }
    pub async fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        self.inner.lock().await.recv(buf).await
    }
}
use tokio::sync::Mutex;
// use std::sync::Mutex;
pub struct KcpStream {
    session: Arc<KcpSession>,
    receiver: Arc<Mutex<Receiver>>,
    transmitter: Arc<Mutex<Transmitter>>,
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        self.session.close();
    }
}

impl Debug for KcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        async_run!(async {
            let lock = self.receiver.lock().await;
            f.debug_struct("KcpStream")
                .field("session", self.session.as_ref())
                .field("recv_buffer.len", &lock.buffer.len())
                .field("recv_buffer_pos", &lock.buffer_pos)
                .field("recv_buffer_cap", &lock.buffer_cap)
                .finish()
        })
    }
}

impl KcpStream {
    /// Create a `KcpStream` connecting to `addr`
    pub async fn connect(config: &KcpConfig, addr: SocketAddr) -> KcpResult<KcpStream> {
        let udp = match addr.ip() {
            IpAddr::V4(..) => UdpSocket::bind("0.0.0.0:0").await?,
            IpAddr::V6(..) => UdpSocket::bind("[::]:0").await?,
        };

        KcpStream::connect_with_socket(config, udp, addr).await
    }

    /// Create a `KcpStream` with an existed `UdpSocket` connecting to `addr`
    pub async fn connect_with_socket(config: &KcpConfig, udp: UdpSocket, addr: SocketAddr) -> KcpResult<KcpStream> {
        let udp = Arc::new(udp);
        let conv = rand::random();
        let socket = KcpSocket::new(config, conv, udp, addr, config.stream)?;

        let session = KcpSession::new_shared(socket, config.session_expire, None);

        Ok(KcpStream::with_session(session))
    }

    pub(crate) fn with_session(session: Arc<KcpSession>) -> KcpStream {
        KcpStream {
            session: session.clone(),
            receiver: Arc::new(Mutex::new(Receiver::new(session.clone()))),
            transmitter: Arc::new(Mutex::new(Transmitter::new(session))),
        }
    }
    pub fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<KcpResult<usize>> {
        async_run!(async {self.transmitter.lock().await.poll_send(cx, buf)})
    }
    pub async fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        self.transmitter.lock().await.send(buf).await
    }

    pub async fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        self.receiver.lock().await.recv(buf).await
    }
    pub fn poll_recv(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<KcpResult<usize>> {
        async_run!(async {self.receiver.lock().await.poll_recv(cx, buf)})
    }

    pub fn split_owned(&self) -> (OwnedWriteHalf, OwnedReadHalf) {
        (
            OwnedWriteHalf {
                inner: self.transmitter.clone(),
            },
            OwnedReadHalf {
                inner: self.receiver.clone(),
            },
        )
    }
}

impl AsyncRead for KcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match ready!(async_run!(async{ self.receiver.lock().await.poll_recv(cx, buf.initialize_unfilled())})) {
            Ok(n) => {
                buf.advance(n);
                Ok(()).into()
            }
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }
}

impl AsyncWrite for KcpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match ready!(async_run!(async {self.transmitter.lock().await.poll_send(cx, buf)})) {
            Ok(n) => Ok(n).into(),
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Mutex doesn't have poll_lock, spinning on it.
        let mut kcp = self.session.kcp_socket().lock();
        match kcp.flush() {
            Ok(..) => {
                self.session.notify();
                Ok(()).into()
            }
            Err(KcpError::IoError(err)) => Err(err).into(),
            Err(err) => Err(io::Error::new(ErrorKind::Other, err)).into(),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for KcpStream {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        let kcp_socket = self.session.kcp_socket().lock();
        kcp_socket.udp_socket().as_raw_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for KcpStream {
    fn as_raw_socket(&self) -> std::os::windows::prelude::RawSocket {
        let kcp_socket = self.session.kcp_socket().lock();
        kcp_socket.udp_socket().as_raw_socket()
    }
}
