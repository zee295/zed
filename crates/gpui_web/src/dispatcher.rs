use gpui::{
    PlatformDispatcher, Priority, PriorityQueueReceiver, PriorityQueueSender, RunnableVariant,
};
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use web_time::Instant;

#[cfg(feature = "multithreaded")]
const MIN_BACKGROUND_THREADS: usize = 2;

#[cfg(feature = "multithreaded")]
fn shared_memory_supported() -> bool {
    let global = js_sys::global();
    let has_shared_array_buffer =
        js_sys::Reflect::has(&global, &JsValue::from_str("SharedArrayBuffer")).unwrap_or(false);
    let has_atomics = js_sys::Reflect::has(&global, &JsValue::from_str("Atomics")).unwrap_or(false);
    if !has_shared_array_buffer || !has_atomics {
        return false;
    }
    // Thread build: memory is *imported* into the wasm (not in exports), so
    // `wasm_bindgen::memory()` is undefined. The glue stashes the real shared
    // memory on `globalThis.__wbgSharedMemory`; check its buffer directly.
    if let Ok(stash) = js_sys::Reflect::get(&global, &JsValue::from_str("__wbgSharedMemory")) {
        if !stash.is_undefined() && !stash.is_null() {
            if let Ok(buffer) = js_sys::Reflect::get(&stash, &JsValue::from_str("buffer")) {
                return buffer.is_instance_of::<js_sys::SharedArrayBuffer>();
            }
        }
    }
    let buffer = js_sys::WebAssembly::Memory::from(wasm_bindgen::memory()).buffer();
    buffer.is_instance_of::<js_sys::SharedArrayBuffer>()
}

enum MainThreadItem {
    Runnable(RunnableVariant),
    Delayed {
        runnable: RunnableVariant,
        millis: i32,
    },
    // TODO-Wasm: Shouldn't these run on their own dedicated thread?
    RealtimeFunction(Box<dyn FnOnce() + Send>),
}

struct MainThreadMailbox {
    sender: PriorityQueueSender<MainThreadItem>,
    receiver: parking_lot::Mutex<PriorityQueueReceiver<MainThreadItem>>,
    signal: AtomicI32,
}

impl MainThreadMailbox {
    fn new() -> Self {
        let (sender, receiver) = PriorityQueueReceiver::new();
        Self {
            sender,
            receiver: parking_lot::Mutex::new(receiver),
            signal: AtomicI32::new(0),
        }
    }

    fn post(&self, priority: Priority, item: MainThreadItem) {
        if self.sender.spin_send(priority, item).is_err() {
            log::error!("MainThreadMailbox::send failed: receiver disconnected");
        }

        // TODO-Wasm: Verify this lock-free protocol
        let view = self.signal_view();
        js_sys::Atomics::store(&view, 0, 1).ok();
        js_sys::Atomics::notify(&view, 0).ok();
    }

    fn drain(&self, window: &web_sys::Window) {
        let mut receiver = self.receiver.lock();
        loop {
            // We need these `spin` variants because we can't acquire a lock on the main thread.
            // TODO-WASM: Should we do something different?
            match receiver.spin_try_pop() {
                Ok(Some(item)) => execute_on_main_thread(window, item),
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    fn signal_view(&self) -> js_sys::Int32Array {
        let byte_offset = self.signal.as_ptr() as u32;
        let memory = js_sys::WebAssembly::Memory::from(wasm_bindgen::memory());
        js_sys::Int32Array::new_with_byte_offset_and_length(&memory.buffer(), byte_offset, 1)
    }

    fn run_waker_loop(self: &Arc<Self>, window: web_sys::Window) {
        if !shared_memory_supported() {
            log::warn!("SharedArrayBuffer not available; main thread mailbox waker loop disabled");
            return;
        }

        let mailbox = Arc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let view = mailbox.signal_view();
            loop {
                js_sys::Atomics::store(&view, 0, 0).expect("Atomics.store failed");

                let result = match js_sys::Atomics::wait_async(&view, 0, 0) {
                    Ok(result) => result,
                    Err(error) => {
                        log::error!("Atomics.waitAsync failed: {error:?}");
                        break;
                    }
                };

                let is_async = js_sys::Reflect::get(&result, &JsValue::from_str("async"))
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !is_async {
                    log::error!("Atomics.waitAsync returned synchronously; waker loop exiting");
                    break;
                }

                let promise: js_sys::Promise =
                    js_sys::Reflect::get(&result, &JsValue::from_str("value"))
                        .expect("waitAsync result missing 'value'")
                        .unchecked_into();

                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;

                mailbox.drain(&window);
            }
        });
    }
}

pub struct WebDispatcher {
    main_thread_id: std::thread::ThreadId,
    browser_window: web_sys::Window,
    background_sender: PriorityQueueSender<RunnableVariant>,
    main_thread_mailbox: Arc<MainThreadMailbox>,
    supports_threads: bool,
    #[cfg(feature = "multithreaded")]
    _background_threads: Vec<wasm_thread::JoinHandle<()>>,
}

// Safety: `web_sys::Window` is only accessed from the main thread
// All other fields are `Send + Sync` by construction.
unsafe impl Send for WebDispatcher {}
unsafe impl Sync for WebDispatcher {}

impl WebDispatcher {
    pub fn new(browser_window: web_sys::Window, allow_threads: bool) -> Self {
        #[cfg(feature = "multithreaded")]
        let (background_sender, background_receiver) = PriorityQueueReceiver::new();
        #[cfg(not(feature = "multithreaded"))]
        let (background_sender, _) = PriorityQueueReceiver::new();

        let main_thread_mailbox = Arc::new(MainThreadMailbox::new());

        #[cfg(feature = "multithreaded")]
        let has_shared_memory = shared_memory_supported();
        #[cfg(feature = "multithreaded")]
        let supports_threads = allow_threads && has_shared_memory;
        #[cfg(not(feature = "multithreaded"))]
        let supports_threads = false;

        #[cfg(feature = "multithreaded")]
        if has_shared_memory {
            main_thread_mailbox.run_waker_loop(browser_window.clone());
        }

        if allow_threads && !supports_threads {
            log::warn!(
                "SharedArrayBuffer not available; falling back to single-threaded dispatcher"
            );
        }

        #[cfg(feature = "multithreaded")]
        let background_threads = if supports_threads {
            let thread_count = browser_window
                .navigator()
                .hardware_concurrency()
                .max(MIN_BACKGROUND_THREADS as f64) as usize;

            // TODO-Wasm: Is it bad to have web workers blocking for a long time like this?
            (0..thread_count)
                .map(|i| {
                    let mut receiver = background_receiver.clone();
                    wasm_thread::Builder::new()
                        .name(format!("background-worker-{i}"))
                        .spawn(move || {
                            loop {
                                let runnable: RunnableVariant = match receiver.pop() {
                                    Ok(runnable) => runnable,
                                    Err(_) => {
                                        log::info!(
                                            "background-worker-{i}: channel disconnected, exiting"
                                        );
                                        break;
                                    }
                                };

                                runnable.run();
                            }
                        })
                        .expect("failed to spawn background worker thread")
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        Self {
            main_thread_id: std::thread::current().id(),
            browser_window,
            background_sender,
            main_thread_mailbox,
            supports_threads,
            #[cfg(feature = "multithreaded")]
            _background_threads: background_threads,
        }
    }

    fn on_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }
}

impl PlatformDispatcher for WebDispatcher {
    fn is_main_thread(&self) -> bool {
        self.on_main_thread()
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        if !self.supports_threads {
            self.dispatch_on_main_thread(runnable, priority);
            return;
        }

        let result = if self.on_main_thread() {
            self.background_sender.spin_send(priority, runnable)
        } else {
            self.background_sender.send(priority, runnable)
        };

        if let Err(error) = result {
            log::error!("dispatch: failed to send to background queue: {error:?}");
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        if self.on_main_thread() {
            schedule_runnable(&self.browser_window, runnable, priority);
        } else {
            self.main_thread_mailbox
                .post(priority, MainThreadItem::Runnable(runnable));
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let millis = duration.as_millis().min(i32::MAX as u128) as i32;
        if self.on_main_thread() {
            let window = self.browser_window.clone();
            let callback = Closure::once_into_js(move || {
                run_when_app_free(&window, runnable);
            });
            self.browser_window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.unchecked_ref(),
                    millis,
                )
                .ok();
        } else {
            self.main_thread_mailbox
                .post(Priority::High, MainThreadItem::Delayed { runnable, millis });
        }
    }

    fn spawn_realtime(&self, function: Box<dyn FnOnce() + Send>) {
        if self.on_main_thread() {
            let window = self.browser_window.clone();
            let callback = Closure::once_into_js(move || {
                if gpui::is_app_borrowed() {
                    let callback = Closure::once_into_js(move || {
                        function();
                    });
                    window
                        .set_timeout_with_callback_and_timeout_and_arguments_0(
                            callback.unchecked_ref(),
                            0,
                        )
                        .ok();
                } else {
                    function();
                }
            });
            // Prefer setTimeout(0) over queueMicrotask so we don't run between
            // nested microtasks while App is still borrowed.
            self.browser_window
                .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), 0)
                .ok();
        } else {
            self.main_thread_mailbox
                .post(Priority::High, MainThreadItem::RealtimeFunction(function));
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

fn execute_on_main_thread(window: &web_sys::Window, item: MainThreadItem) {
    match item {
        MainThreadItem::Runnable(runnable) => {
            run_when_app_free(window, runnable);
        }
        MainThreadItem::Delayed { runnable, millis } => {
            let window_for_cb = window.clone();
            let callback = Closure::once_into_js(move || {
                run_when_app_free(&window_for_cb, runnable);
            });
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.unchecked_ref(),
                    millis,
                )
                .ok();
        }
        MainThreadItem::RealtimeFunction(function) => {
            // Realtime work must not re-enter App either.
            if gpui::is_app_borrowed() {
                let callback = Closure::once_into_js(move || {
                    function();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        callback.unchecked_ref(),
                        0,
                    )
                    .ok();
            } else {
                function();
            }
        }
    }
}

/// Run a main-thread task only when the GPUI App `RefCell` is free.
///
/// On the web, RAF / ResizeObserver / timer completions can interleave. If a
/// foreground task polls an async future that calls `AsyncApp::update_entity`
/// while a frame already holds `App`, we panic with "RefCell already borrowed".
/// Re-queue with `setTimeout(0)` until the outer borrow is released.
fn run_when_app_free(window: &web_sys::Window, runnable: RunnableVariant) {
    if gpui::is_app_borrowed() {
        schedule_runnable(window, runnable, Priority::default());
        return;
    }
    runnable.run();
}

fn schedule_runnable(window: &web_sys::Window, runnable: RunnableVariant, _priority: Priority) {
    let window_for_cb = window.clone();
    let callback = Closure::once_into_js(move || {
        run_when_app_free(&window_for_cb, runnable);
    });
    let callback: &js_sys::Function = callback.unchecked_ref();

    // Always use setTimeout(0) (macrotask). queueMicrotask can run between
    // nested microtasks while App is still borrowed from a parent sync stack.
    // TODO-Wasm: enqueue so we can dequeue with proper priority
    window
        .set_timeout_with_callback_and_timeout_and_arguments_0(callback, 0)
        .ok();
}
