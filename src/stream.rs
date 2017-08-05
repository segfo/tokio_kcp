use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use bytes::BytesMut;
use futures::{Async, Poll};
use rand;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented};
use tokio_io::{AsyncRead, AsyncWrite};

use config::KcpConfig;
use kcp_io::{ClientKcpIo, ServerKcpIo};
use session::KcpSessionUpdater;
use skcp::SharedKcp;

/// KCP client for interacting with server
pub struct KcpStream {
    udp: Rc<UdpSocket>,
    io: ClientKcpIo,
    buf: BytesMut,
}

impl KcpStream {
    #[doc(hidden)]
    pub fn new(udp: Rc<UdpSocket>, io: ClientKcpIo) -> KcpStream {
        let mut buf = BytesMut::with_capacity(io.mtu());
        unsafe {
            buf.set_len(io.mtu());
        }
        KcpStream {
            udp: udp,
            io: io,
            buf: buf,
        }
    }

    /// Opens a KCP connection to a remote host.
    pub fn connect(addr: &SocketAddr, handle: &Handle) -> io::Result<KcpStream> {
        KcpStream::connect_with_config(addr, handle, &KcpConfig::default())
    }

    /// Opens a KCP connection to a remote host.
    pub fn connect_with_config(addr: &SocketAddr, handle: &Handle, config: &KcpConfig) -> io::Result<KcpStream> {
        let local = SocketAddr::new(IpAddr::from(Ipv4Addr::new(0, 0, 0, 0)), 0);

        let udp = UdpSocket::bind(&local, &handle)?;
        let udp = Rc::new(udp);

        let kcp = SharedKcp::new_with_config(config, rand::random::<u32>(), udp.clone(), *addr, handle);

        let sess_exp = match config.session_expire {
            Some(dur) => dur,
            None => Duration::from_secs(90),
        };

        let io = ClientKcpIo::new(kcp, *addr, sess_exp, &handle)?;
        Ok(KcpStream::new(udp, io))
    }
}

impl Read for KcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        for lp in 1.. {
            match self.udp.recv_from(&mut self.buf) {
                Ok((n, addr)) => {
                    trace!("[RECV] UDP {} size={} {:?}", addr, n, ::debug::BsDebug(&self.buf[..n]));
                    self.io.input(&self.buf[..n])?;
                }
                Err(err) => {
                    if err.kind() == ErrorKind::WouldBlock {
                        if lp > 1 {
                            return Err(err);
                        }
                    } else {
                        return Err(err);
                    }
                }
            }

            let n = match self.io.read(buf) {
                Ok(n) => {
                    trace!("[RECV] Evented.read size={} {:?}", n, ::debug::BsDebug(&buf[..n]));
                    n
                }
                // Loop continue, maybe we received an ACK packet
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => continue,
                Err(err) => return Err(err),
            };

            return Ok(n);
        }

        unreachable!()
    }
}

impl AsyncRead for KcpStream {}

impl AsyncWrite for KcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

impl Write for KcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

/// KCP client between a local and remote socket
///
/// After creating a `KcpStream` by either connecting to a remote host or accepting a connection on a `KcpListener`,
/// data can be transmitted by reading and writing to it.
pub struct ServerKcpStream {
    io: PollEvented<ServerKcpIo>,
}

impl ServerKcpStream {
    #[doc(hidden)]
    pub fn new_with_config(conv: u32,
                           udp: Rc<UdpSocket>,
                           addr: &SocketAddr,
                           handle: &Handle,
                           u: &mut KcpSessionUpdater,
                           config: &KcpConfig)
                           -> io::Result<ServerKcpStream> {
        let kcp = SharedKcp::new_with_config(&config, conv, udp.clone(), *addr, handle);

        let sess_exp = match config.session_expire {
            Some(dur) => dur,
            None => Duration::from_secs(90),
        };

        let io = ServerKcpIo::new(kcp, *addr, sess_exp, handle, u)?;
        let io = PollEvented::new(io, handle)?;
        Ok(ServerKcpStream { io: io })

    }

    #[doc(hidden)]
    pub fn input(&mut self, buf: &[u8]) -> io::Result<()> {
        let io = self.io.get_mut();
        io.input(buf)
    }
}

impl Read for ServerKcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for ServerKcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // FIXME: Write does not have events yet
        self.io.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // FIXME: Write does not have events yet
        self.io.get_mut().flush()
    }
}

impl AsyncRead for ServerKcpStream {}

impl AsyncWrite for ServerKcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.io.get_mut().shutdown()?;
        Ok(Async::Ready(()))
    }
}
