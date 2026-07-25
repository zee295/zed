use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex,
};

use futures::future::poll_fn;

use super::utils::SpinLockMutex;

/// A combined sync/async synchronization primitive that allows waiting for a condition.
pub struct Signal {
    waiters: Mutex<Vec<std::task::Waker>>,
    // Starts with 0 and changes to 1 when signaled
    value: AtomicU32,
}

impl Signal {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Default::default()),
            value: AtomicU32::new(0),
        }
    }

    /// Sends a signal and unlocks all waiters.
    pub fn signal(&self) {
        self.value.store(1, Ordering::SeqCst);

        // Wake all async waiters
        for waiter in self.waiters.lock_spin().unwrap().drain(..) {
            waiter.wake();
        }
    }

    /// Synchronously waits until [Self::signal] is called.
    ///
    /// On wasm32-unknown-unknown there are no real threads, so a blocking wait
    /// cannot be satisfied by another thread. We simply yield/spin briefly and
    /// return so that the crate compiles on stable Rust without the unstable
    /// `stdarch_wasm_atomic_wait` feature.
    pub fn wait(&self) {
        while self.value.load(Ordering::Relaxed) == 0 {
            std::hint::spin_loop();
        }
    }

    /// Asynchronously waits until [Self::signal] is called.
    pub async fn wait_async(&self) {
        poll_fn(|cx| {
            self.waiters.lock_spin().unwrap().push(cx.waker().clone());

            if self.value.load(Ordering::Relaxed) == 1 {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }
}
