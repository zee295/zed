//! WASM stub for `smol::net`.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "net not supported on WASM",
    ))
}

pub struct TcpStream;

impl TcpStream {
    pub async fn connect(_addr: impl std::net::ToSocketAddrs) -> io::Result<TcpStream> {
        unsupported()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }
}

impl futures_lite::io::AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(unsupported())
    }
}

impl futures_lite::io::AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(unsupported())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub struct TcpListener;

impl TcpListener {
    pub async fn bind(_addr: impl std::net::ToSocketAddrs) -> io::Result<TcpListener> {
        unsupported()
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        unsupported()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }
}

pub struct UdpSocket;

impl UdpSocket {
    pub async fn bind(_addr: impl std::net::ToSocketAddrs) -> io::Result<UdpSocket> {
        unsupported()
    }

    pub async fn send_to(&self, _buf: &[u8], _addr: impl Into<SocketAddr>) -> io::Result<usize> {
        unsupported()
    }

    pub async fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }
}

#[cfg(unix)]
pub mod unix {
    use std::io;
    use std::path::Path;

    pub struct UnixListener;

    impl UnixListener {
        pub async fn bind(_path: impl AsRef<Path>) -> io::Result<UnixListener> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unix sockets not supported on WASM",
            ))
        }

        pub async fn accept(&self) -> io::Result<(UnixStream, ())> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unix sockets not supported on WASM",
            ))
        }
    }

    pub struct UnixStream;

    impl UnixStream {
        pub async fn connect(_path: impl AsRef<Path>) -> io::Result<UnixStream> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unix sockets not supported on WASM",
            ))
        }
    }
}
