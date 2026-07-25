//! WASM-compatible wrapper around the `smol` async runtime.
//!
//! On native targets this crate re-exports the real `smol` building blocks.
//! On `wasm32-unknown-unknown` it provides stubs for the OS-dependent pieces
//! (filesystem, networking, processes, blocking thread pool) while keeping the
//! pure-Rust pieces (`channel`, `lock`, `future`, `io`, `stream`, etc.).

pub use async_channel as channel;
pub use async_executor::{Executor, LocalExecutor, Task};
pub use async_lock as lock;
pub use futures_lite::{future, io, pin, prelude, ready, stream};

#[cfg(not(target_family = "wasm"))]
#[doc(inline)]
pub use {
    async_fs as fs,
    async_io::{Async, Timer, block_on},
    async_net as net, async_process as process,
    blocking::{Unblock, unblock},
};

#[cfg(target_family = "wasm")]
pub mod fs;
#[cfg(target_family = "wasm")]
pub mod net;
#[cfg(target_family = "wasm")]
pub mod process;
#[cfg(target_family = "wasm")]
pub use process::set_remote_client;

#[cfg(target_family = "wasm")]
pub use wasm::{Async, Timer, Unblock, block_on, unblock};

mod spawn;
pub use spawn::spawn;

#[cfg(target_family = "wasm")]
mod wasm {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// A timer that never fires on WASM.
    #[derive(Debug)]
    pub struct Timer;

    impl Timer {
        /// Creates a timer that never fires.
        pub fn after(_duration: Duration) -> Timer {
            Timer
        }

        /// Creates a timer that never fires.
        pub fn interval(_period: Duration) -> Timer {
            Timer
        }

        /// Sets a new duration. Still never fires.
        pub fn set_after(&mut self, _duration: Duration) {}

        /// Sets a new period. Still never fires.
        pub fn set_interval(&mut self, _period: Duration) {}
    }

    impl Future for Timer {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Future for &Timer {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    /// Wraps an I/O type so it implements async I/O traits. On WASM all
    /// operations return `Unsupported`.
    #[derive(Debug)]
    pub struct Async<T>(pub T);

    impl<T> Async<T> {
        /// Creates a new async I/O handle. On WASM this simply wraps the value.
        pub fn new(io: T) -> io::Result<Async<T>> {
            Ok(Async(io))
        }

        /// Returns a shared reference to the inner object.
        pub fn get_ref(&self) -> &T {
            &self.0
        }

        /// Returns a mutable reference to the inner object.
        pub fn get_mut(&mut self) -> &mut T {
            &mut self.0
        }

        /// Destroys the async wrapper and returns the inner object.
        pub fn into_inner(self) -> T {
            self.0
        }

        /// Waits until the object is readable. Always pending on WASM.
        pub async fn readable(&self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "WASM"))
        }

        /// Waits until the object is writable. Always pending on WASM.
        pub async fn writable(&self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "WASM"))
        }
    }

    /// Runs a future on the current thread. On WASM this panics because
    /// blocking is not available in the browser.
    pub fn block_on<T>(_future: impl Future<Output = T>) -> T {
        panic!("smol::block_on is not supported on WASM")
    }

    /// Offloads a blocking operation to a thread pool. On WASM it runs the
    /// closure synchronously and returns the result.
    pub async fn unblock<T>(task: impl FnOnce() -> T) -> T {
        task()
    }

    /// Adapter that makes a blocking I/O object async. On WASM it wraps the
    /// value and every async operation returns `Unsupported`.
    #[derive(Debug)]
    pub struct Unblock<T>(T);

    impl<T> Unblock<T> {
        /// Creates a new unblocking adapter.
        pub fn new(io: T) -> Unblock<T> {
            Unblock(io)
        }

        /// Returns a shared reference to the inner object.
        pub fn get_ref(&self) -> &T {
            &self.0
        }

        /// Returns a mutable reference to the inner object.
        pub fn get_mut(&mut self) -> &mut T {
            &mut self.0
        }

        /// Destroys the adapter and returns the inner object.
        pub fn into_inner(self) -> T {
            self.0
        }
    }
}
