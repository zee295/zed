//! Global `spawn` function.

use std::future::Future;

/// Spawns a task on the global executor.
#[cfg(not(target_family = "wasm"))]
pub fn spawn<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> async_executor::Task<T> {
    use async_executor::Executor;
    use async_lock::OnceCell;
    use std::panic::catch_unwind;
    use std::thread;

    static GLOBAL: OnceCell<Executor<'_>> = OnceCell::new();

    fn global() -> &'static Executor<'static> {
        GLOBAL.get_or_init_blocking(|| {
            let num_threads = std::env::var("SMOL_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);

            for n in 1..=num_threads {
                thread::Builder::new()
                    .name(format!("smol-{}", n))
                    .spawn(|| {
                        loop {
                            catch_unwind(|| {
                                async_io::block_on(
                                    global().run(futures_lite::future::pending::<()>()),
                                )
                            })
                            .ok();
                        }
                    })
                    .expect("cannot spawn executor thread");
            }

            let ex = Executor::new();
            #[cfg(not(target_os = "espidf"))]
            ex.spawn(async_process::driver()).detach();
            ex
        })
    }

    global().spawn(future)
}

/// WASM stub: returns a task that never resolves.
#[cfg(target_family = "wasm")]
pub fn spawn<T: 'static>(future: impl Future<Output = T> + 'static) -> WasmTask<T> {
    // Drop the future without running it.
    let _ = Box::new(future);
    WasmTask {
        _phantom: std::marker::PhantomData,
    }
}

#[cfg(target_family = "wasm")]
#[derive(Debug)]
pub struct WasmTask<T> {
    _phantom: std::marker::PhantomData<T>,
}

#[cfg(target_family = "wasm")]
impl<T> WasmTask<T> {
    pub fn detach(self) {}

    pub async fn cancel(self) -> Option<T> {
        None
    }
}

#[cfg(target_family = "wasm")]
impl<T> Future for WasmTask<T> {
    type Output = T;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Pending
    }
}
