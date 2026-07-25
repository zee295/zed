pub mod async_net;
#[cfg(target_os = "windows")]
pub mod listener;
#[cfg(target_os = "windows")]
pub mod socket;
#[cfg(target_os = "windows")]
pub mod stream;
#[cfg(target_os = "windows")]
mod util;

#[cfg(target_os = "windows")]
pub use listener::*;
#[cfg(target_os = "windows")]
pub use socket::*;
#[cfg(all(not(target_os = "windows"), not(target_family = "wasm")))]
pub use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(target_family = "wasm")]
pub use wasm::*;

#[cfg(target_family = "wasm")]
pub mod wasm {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix sockets are not supported on wasm",
        )
    }

    #[derive(Debug)]
    pub struct UnixListener;

    impl UnixListener {
        pub fn bind<P: AsRef<Path>>(_path: P) -> io::Result<Self> {
            Err(unsupported())
        }
        pub async fn accept(&self) -> io::Result<(UnixStream, ())> {
            Err(unsupported())
        }
    }

    #[derive(Debug)]
    pub struct UnixStream;

    impl UnixStream {
        pub fn connect<P: AsRef<Path>>(_path: P) -> io::Result<Self> {
            Err(unsupported())
        }
        pub fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
        pub fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        pub fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Read for UnixStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.read(buf)
        }
    }

    impl std::io::Write for UnixStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flush()
        }
    }

    impl futures::io::AsyncRead for UnixStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }
    }

    impl futures::io::AsyncWrite for UnixStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
#[cfg(target_os = "windows")]
pub use stream::*;

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use smol::io::{AsyncReadExt, AsyncWriteExt};

    const SERVER_MESSAGE: &str = "Connection closed";
    const CLIENT_MESSAGE: &str = "Hello, server!";
    const BUFFER_SIZE: usize = 32;

    #[test]
    fn test_windows_listener() -> std::io::Result<()> {
        use crate::{UnixListener, UnixStream};

        let temp = tempfile::tempdir()?;
        let socket = temp.path().join("socket.sock");
        let listener = UnixListener::bind(&socket)?;

        // Server
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();

            // Read data from the client
            let mut buffer = [0; BUFFER_SIZE];
            let bytes_read = stream.read(&mut buffer).unwrap();
            let string = String::from_utf8_lossy(&buffer[..bytes_read]);
            assert_eq!(string, CLIENT_MESSAGE);

            // Send a message back to the client
            stream.write_all(SERVER_MESSAGE.as_bytes()).unwrap();
        });

        // Client
        let mut client = UnixStream::connect(&socket)?;

        // Send data to the server
        client.write_all(CLIENT_MESSAGE.as_bytes())?;
        let mut buffer = [0; BUFFER_SIZE];

        // Read the response from the server
        let bytes_read = client.read(&mut buffer)?;
        let string = String::from_utf8_lossy(&buffer[..bytes_read]);
        assert_eq!(string, SERVER_MESSAGE);
        client.flush()?;

        server.join().unwrap();
        Ok(())
    }

    #[test]
    fn test_unix_listener() -> std::io::Result<()> {
        use crate::async_net::{UnixListener, UnixStream};

        smol::block_on(async {
            let temp = tempfile::tempdir()?;
            let socket = temp.path().join("socket.sock");
            let listener = UnixListener::bind(&socket)?;

            // Server
            let server = smol::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();

                // Read data from the client
                let mut buffer = [0; BUFFER_SIZE];
                let bytes_read = stream.read(&mut buffer).await.unwrap();
                let string = String::from_utf8_lossy(&buffer[..bytes_read]);
                assert_eq!(string, CLIENT_MESSAGE);

                // Send a message back to the client
                stream.write_all(SERVER_MESSAGE.as_bytes()).await.unwrap();
            });

            // Client
            let mut client = UnixStream::connect(&socket).await?;
            client.write_all(CLIENT_MESSAGE.as_bytes()).await?;

            // Read the response from the server
            let mut buffer = [0; BUFFER_SIZE];
            let bytes_read = client.read(&mut buffer).await?;
            let string = String::from_utf8_lossy(&buffer[..bytes_read]);
            assert_eq!(string, "Connection closed");
            client.flush().await?;

            server.await;
            Ok(())
        })
    }
}
