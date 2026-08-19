use crate::display::WebDisplay;
use crate::events::{
    ClickState, EventListenerHandle, TouchMomentumState, TouchPointerState, WebEventListeners,
    is_mac_platform,
};
use crate::platform::WebWindowLifecycle;
use std::sync::Arc;
use std::{cell::Cell, cell::RefCell, rc::Rc};

use gpui::{
    AnyWindowHandle, Bounds, Capslock, ClipboardItem, Decorations, DevicePixels,
    DispatchEventResult, GpuSpecs, Modifiers, MouseButton, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel,
    RequestFrameOptions, ResizeEdge, Scene, Size, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, WindowControls, WindowDecorations, WindowParams, px,
};
use gpui_wgpu::{WgpuContext, WgpuRenderer, WgpuSurfaceConfig, wgpu};
use wasm_bindgen::prelude::*;

#[derive(Default)]
pub(crate) struct WebWindowCallbacks {
    pub(crate) request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    pub(crate) input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    pub(crate) active_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) hover_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    pub(crate) moved: Option<Box<dyn FnMut()>>,
    pub(crate) should_close: Option<Box<dyn FnMut() -> bool>>,
    pub(crate) close: Option<Box<dyn FnOnce()>>,
    pub(crate) appearance_changed: Option<Box<dyn FnMut()>>,
    pub(crate) hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
}

pub(crate) struct WebWindowMutableState {
    pub(crate) renderer: WgpuRenderer,
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) scale_factor: f32,
    pub(crate) max_texture_dimension: u32,
    pub(crate) title: String,
    pub(crate) input_handler: Option<PlatformInputHandler>,
    pub(crate) is_fullscreen: bool,
    pub(crate) is_active: bool,
    pub(crate) is_hovered: bool,
    pub(crate) mouse_position: Point<Pixels>,
    pub(crate) modifiers: Modifiers,
    pub(crate) capslock: Capslock,
}

pub(crate) struct WebWindowInner {
    pub(crate) browser_window: web_sys::Window,
    pub(crate) canvas: web_sys::HtmlCanvasElement,
    pub(crate) input_element: web_sys::HtmlTextAreaElement,
    pub(crate) has_device_pixel_support: bool,
    pub(crate) is_mac: bool,
    pub(crate) state: RefCell<WebWindowMutableState>,
    pub(crate) callbacks: RefCell<WebWindowCallbacks>,
    pub(crate) click_state: RefCell<ClickState>,
    pub(crate) pressed_button: Cell<Option<MouseButton>>,
    pub(crate) active_touch: RefCell<Option<TouchPointerState>>,
    pub(crate) touch_momentum: Cell<Option<TouchMomentumState>>,
    pub(crate) soft_keyboard_requested: Cell<bool>,
    pub(crate) last_physical_size: Cell<(u32, u32)>,
    pub(crate) notify_scale: Cell<bool>,
    pub(crate) is_composing: Cell<bool>,
    pub(crate) native_text_input: RefCell<NativeTextInputState>,
    pub(crate) uses_native_text_input: bool,
    pub(crate) pending_clipboard: Rc<RefCell<Option<ClipboardItem>>>,
    pub(crate) last_cursor_css: Rc<Cell<&'static str>>,
    keyboard_accessory: Option<KeyboardAccessory>,
    keyboard_accessory_expanded: Cell<bool>,
    keyboard_accessory_modifiers: Cell<Modifiers>,
    mql_handle: RefCell<Option<MqlHandle>>,
    pending_physical_size: Cell<Option<(u32, u32)>>,
    raf_id: Cell<Option<i32>>,
}

#[derive(Default)]
pub(crate) struct NativeTextInputState {
    pub(crate) value: String,
    pub(crate) document_start_utf16: Option<usize>,
    pub(crate) mode: NativeTextInputMode,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) enum NativeTextInputMode {
    #[default]
    Inactive,
    Document,
    Terminal,
}

struct KeyboardAccessory {
    root: web_sys::HtmlElement,
    key_row: web_sys::HtmlElement,
    toggle: web_sys::HtmlElement,
    control: web_sys::HtmlElement,
    alt: web_sys::HtmlElement,
}

pub struct WebWindow {
    inner: Rc<WebWindowInner>,
    display: Rc<dyn PlatformDisplay>,
    lifecycle: Rc<Cell<WebWindowLifecycle>>,
    active_window: Rc<RefCell<Option<AnyWindowHandle>>>,
    _raf_closure: Closure<dyn FnMut(f64)>,
    _resize_observer: Option<web_sys::ResizeObserver>,
    _resize_observer_closure: Closure<dyn FnMut(js_sys::Array)>,
    _event_listeners: WebEventListeners,
}

impl WebWindow {
    pub(crate) fn prepare_canvas(
        browser_window: &web_sys::Window,
    ) -> anyhow::Result<web_sys::HtmlCanvasElement> {
        let document = browser_window
            .document()
            .ok_or_else(|| anyhow::anyhow!("No `document` found on window"))?;
        let canvas: web_sys::HtmlCanvasElement = document
            .create_element("canvas")
            .map_err(|error| anyhow::anyhow!("Failed to create canvas element: {error:?}"))?
            .dyn_into()
            .map_err(|error| anyhow::anyhow!("Created element is not a canvas: {error:?}"))?;
        canvas.set_tab_index(-1);

        let style = canvas.style();
        for (property, value) in [
            ("width", "100%"),
            ("height", "100%"),
            ("display", "block"),
            ("outline", "none"),
            ("touch-action", "none"),
            ("-webkit-tap-highlight-color", "transparent"),
            ("-webkit-touch-callout", "none"),
            ("-webkit-user-select", "none"),
            ("user-select", "none"),
        ] {
            style.set_property(property, value).map_err(|error| {
                anyhow::anyhow!("Failed to set canvas {property} style: {error:?}")
            })?;
        }

        let body = document
            .body()
            .ok_or_else(|| anyhow::anyhow!("No `body` found on document"))?;
        body.append_child(&canvas)
            .map_err(|error| anyhow::anyhow!("Failed to append canvas to body: {error:?}"))?;
        Ok(canvas)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        _handle: AnyWindowHandle,
        _params: WindowParams,
        context: &WgpuContext,
        canvas: web_sys::HtmlCanvasElement,
        surface: wgpu::Surface<'static>,
        browser_window: web_sys::Window,
        lifecycle: Rc<Cell<WebWindowLifecycle>>,
        active_window: Rc<RefCell<Option<AnyWindowHandle>>>,
        pending_clipboard: Rc<RefCell<Option<ClipboardItem>>>,
        last_cursor_css: Rc<Cell<&'static str>>,
    ) -> anyhow::Result<Self> {
        let document = browser_window
            .document()
            .ok_or_else(|| anyhow::anyhow!("No `document` found on window"))?;
        let body = document
            .body()
            .ok_or_else(|| anyhow::anyhow!("No `body` found on document"))?;
        let dpr = browser_window.device_pixel_ratio() as f32;
        let max_texture_dimension = context.device.limits().max_texture_dimension_2d;
        let has_device_pixel_support = check_device_pixel_support();
        let renderer_config = WgpuSurfaceConfig {
            size: Size {
                width: DevicePixels(0),
                height: DevicePixels(0),
            },
            transparent: false,
            preferred_present_mode: None,
        };
        let renderer = WgpuRenderer::new_from_surface(context, surface, renderer_config)?;

        let input_element: web_sys::HtmlTextAreaElement = document
            .create_element("textarea")
            .map_err(|e| anyhow::anyhow!("Failed to create textarea element: {e:?}"))?
            .dyn_into()
            .map_err(|e| anyhow::anyhow!("Created element is not a textarea: {e:?}"))?;
        let input_style = input_element.style();
        input_style.set_property("position", "fixed").ok();
        input_style.set_property("top", "0").ok();
        input_style.set_property("left", "0").ok();
        input_style.set_property("width", "1px").ok();
        input_style.set_property("height", "1px").ok();
        input_style.set_property("opacity", "0").ok();
        input_style.set_property("font-size", "16px").ok();
        input_style.set_property("resize", "none").ok();
        input_element.set_wrap("off");
        body.append_child(&input_element)
            .map_err(|e| anyhow::anyhow!("Failed to append input to body: {e:?}"))?;
        input_element.focus().ok();

        let uses_native_text_input = browser_window
            .match_media("(any-pointer: coarse)")
            .ok()
            .flatten()
            .is_some_and(|query| query.matches());
        let keyboard_accessory =
            create_mobile_keyboard_accessory(&document, &browser_window, &body)?;
        let display: Rc<dyn PlatformDisplay> = Rc::new(WebDisplay::new(browser_window.clone()));

        let initial_bounds = Bounds {
            origin: Point::default(),
            size: Size::default(),
        };

        let mutable_state = WebWindowMutableState {
            renderer,
            bounds: initial_bounds,
            scale_factor: dpr,
            max_texture_dimension,
            title: String::new(),
            input_handler: None,
            is_fullscreen: false,
            is_active: true,
            is_hovered: false,
            mouse_position: Point::default(),
            modifiers: Modifiers::default(),
            capslock: Capslock::default(),
        };

        let is_mac = is_mac_platform(&browser_window);

        let inner = Rc::new(WebWindowInner {
            browser_window,
            canvas,
            input_element,
            has_device_pixel_support,
            is_mac,
            state: RefCell::new(mutable_state),
            callbacks: RefCell::new(WebWindowCallbacks::default()),
            click_state: RefCell::new(ClickState::default()),
            pressed_button: Cell::new(None),
            active_touch: RefCell::new(None),
            touch_momentum: Cell::new(None),
            soft_keyboard_requested: Cell::new(false),
            last_physical_size: Cell::new((0, 0)),
            notify_scale: Cell::new(false),
            is_composing: Cell::new(false),
            native_text_input: RefCell::new(NativeTextInputState::default()),
            uses_native_text_input,
            pending_clipboard,
            last_cursor_css,
            keyboard_accessory,
            keyboard_accessory_expanded: Cell::new(false),
            keyboard_accessory_modifiers: Cell::new(Modifiers::default()),
            mql_handle: RefCell::new(None),
            pending_physical_size: Cell::new(None),
            raf_id: Cell::new(None),
        });

        let raf_closure = inner.create_raf_closure();
        inner.schedule_raf(&raf_closure);

        let resize_observer_closure = Self::create_resize_observer_closure(Rc::clone(&inner));
        let resize_observer =
            web_sys::ResizeObserver::new(resize_observer_closure.as_ref().unchecked_ref()).ok();

        if let Some(ref observer) = resize_observer {
            inner.observe_canvas(observer);
            inner.watch_dpr_changes(observer);
        }

        let event_listeners = inner.register_event_listeners();

        Ok(Self {
            inner,
            display,
            lifecycle,
            active_window,
            _raf_closure: raf_closure,
            _resize_observer: resize_observer,
            _resize_observer_closure: resize_observer_closure,
            _event_listeners: event_listeners,
        })
    }

    fn create_resize_observer_closure(
        inner: Rc<WebWindowInner>,
    ) -> Closure<dyn FnMut(js_sys::Array)> {
        Closure::new(move |entries: js_sys::Array| {
            let entry: web_sys::ResizeObserverEntry = match entries.get(0).dyn_into().ok() {
                Some(entry) => entry,
                None => return,
            };

            let dpr = inner.browser_window.device_pixel_ratio();
            let dpr_f32 = dpr as f32;

            let (physical_width, physical_height, logical_width, logical_height) =
                if inner.has_device_pixel_support {
                    let size: web_sys::ResizeObserverSize = entry
                        .device_pixel_content_box_size()
                        .get(0)
                        .unchecked_into();
                    let pw = size.inline_size() as u32;
                    let ph = size.block_size() as u32;
                    let lw = pw as f64 / dpr;
                    let lh = ph as f64 / dpr;
                    (pw, ph, lw as f32, lh as f32)
                } else {
                    // Safari fallback: use contentRect (always CSS px).
                    let rect = entry.content_rect();
                    let lw = rect.width() as f32;
                    let lh = rect.height() as f32;
                    let pw = (lw as f64 * dpr).round() as u32;
                    let ph = (lh as f64 * dpr).round() as u32;
                    (pw, ph, lw, lh)
                };

            let scale_changed = inner.notify_scale.replace(false);
            let prev = inner.last_physical_size.get();
            let size_changed = prev != (physical_width, physical_height);

            if !scale_changed && !size_changed {
                return;
            }
            inner
                .last_physical_size
                .set((physical_width, physical_height));

            // Skip rendering to a zero-size canvas (e.g. display:none).
            if physical_width == 0 || physical_height == 0 {
                {
                    let mut s = inner.state.borrow_mut();
                    s.bounds.size = Size::default();
                    s.scale_factor = dpr_f32;
                }
                // Still fire the callback so GPUI knows the window is gone.
                inner.with_callback(
                    |callbacks| &mut callbacks.resize,
                    |callback| callback(Size::default(), dpr_f32),
                );
                return;
            }

            let max_texture_dimension = inner.state.borrow().max_texture_dimension;
            let clamped_width = physical_width.min(max_texture_dimension);
            let clamped_height = physical_height.min(max_texture_dimension);

            // Recompute the logical size from the clamped physical size so
            // that scale_factor still maps GPUI's logical bounds exactly onto
            // the surface; otherwise clamping would silently distort the
            // effective scale.
            let (logical_width, logical_height) =
                if (clamped_width, clamped_height) != (physical_width, physical_height) {
                    (
                        (clamped_width as f64 / dpr) as f32,
                        (clamped_height as f64 / dpr) as f32,
                    )
                } else {
                    (logical_width, logical_height)
                };

            inner
                .pending_physical_size
                .set(Some((clamped_width, clamped_height)));

            {
                let mut s = inner.state.borrow_mut();
                s.bounds.size = Size {
                    width: px(logical_width),
                    height: px(logical_height),
                };
                s.scale_factor = dpr_f32;
            }

            let new_size = Size {
                width: px(logical_width),
                height: px(logical_height),
            };

            inner.with_callback(
                |callbacks| &mut callbacks.resize,
                |callback| callback(new_size, dpr_f32),
            );
        })
    }
}

impl WebWindowInner {
    /// Invokes a registered callback with take/call/restore semantics.
    ///
    /// The callback is removed from the slot for the duration of the call, so
    /// the `RefCell` is not borrowed while user code runs: a callback that
    /// re-enters the platform window (dispatching input, registering
    /// handlers) would otherwise panic with a `BorrowMutError`. A re-entrant
    /// invocation of the same callback finds the slot empty and is a no-op.
    pub(crate) fn with_callback<C, R>(
        &self,
        select: impl Fn(&mut WebWindowCallbacks) -> &mut Option<C>,
        invoke: impl FnOnce(&mut C) -> R,
    ) -> Option<R> {
        let mut callback = select(&mut self.callbacks.borrow_mut()).take()?;
        let result = invoke(&mut callback);
        *select(&mut self.callbacks.borrow_mut()) = Some(callback);
        Some(result)
    }

    fn create_raf_closure(self: &Rc<Self>) -> Closure<dyn FnMut(f64)> {
        let raf_handle: Rc<RefCell<Option<js_sys::Function>>> = Rc::new(RefCell::new(None));
        let raf_handle_inner = Rc::clone(&raf_handle);

        let this = Rc::clone(self);
        let closure = Closure::new(move |timestamp| {
            // Momentum shares the platform's frame callback so input cannot
            // re-enter GPUI through an independent animation-frame callback.
            this.tick_touch_momentum(timestamp);
            this.with_callback(
                |callbacks| &mut callbacks.request_frame,
                |callback| {
                    callback(RequestFrameOptions {
                        require_presentation: true,
                        force_render: false,
                    })
                },
            );

            // Re-schedule for the next frame
            if let Some(ref func) = *raf_handle_inner.borrow() {
                this.raf_id
                    .set(this.browser_window.request_animation_frame(func).ok());
            }
        });

        let js_func: js_sys::Function =
            closure.as_ref().unchecked_ref::<js_sys::Function>().clone();
        *raf_handle.borrow_mut() = Some(js_func);

        closure
    }

    fn schedule_raf(&self, closure: &Closure<dyn FnMut(f64)>) {
        self.raf_id.set(
            self.browser_window
                .request_animation_frame(closure.as_ref().unchecked_ref())
                .ok(),
        );
    }

    fn observe_canvas(&self, observer: &web_sys::ResizeObserver) {
        observer.unobserve(&self.canvas);
        if self.has_device_pixel_support {
            let options = web_sys::ResizeObserverOptions::new();
            options.set_box(web_sys::ResizeObserverBoxOptions::DevicePixelContentBox);
            observer.observe_with_options(&self.canvas, &options);
        } else {
            observer.observe(&self.canvas);
        }
    }

    fn watch_dpr_changes(self: &Rc<Self>, observer: &web_sys::ResizeObserver) {
        let current_dpr = self.browser_window.device_pixel_ratio();
        let media_query =
            format!("(resolution: {current_dpr}dppx), (-webkit-device-pixel-ratio: {current_dpr})");
        let Some(mql) = self.browser_window.match_media(&media_query).ok().flatten() else {
            return;
        };

        let this = Rc::clone(self);
        let observer = observer.clone();

        let closure = Closure::<dyn FnMut(JsValue)>::new(move |_event: JsValue| {
            this.notify_scale.set(true);
            this.observe_canvas(&observer);
            this.watch_dpr_changes(&observer);
        });

        mql.add_event_listener_with_callback("change", closure.as_ref().unchecked_ref())
            .ok();

        *self.mql_handle.borrow_mut() = Some(MqlHandle {
            mql,
            _closure: closure,
        });
    }

    pub(crate) fn register_visibility_change(self: &Rc<Self>) -> Option<EventListenerHandle> {
        let document = self.browser_window.document()?;
        let this = Rc::clone(self);

        Some(EventListenerHandle::add(
            document.as_ref(),
            "visibilitychange",
            move |_event: JsValue| {
                let is_visible = this
                    .browser_window
                    .document()
                    .map(|doc| {
                        let state_str: String =
                            js_sys::Reflect::get(&doc, &"visibilityState".into())
                                .ok()
                                .and_then(|v| v.as_string())
                                .unwrap_or_default();
                        state_str == "visible"
                    })
                    .unwrap_or(true);

                if !is_visible {
                    this.cancel_active_touch(None);
                    this.cancel_touch_momentum();
                }

                {
                    let mut state = this.state.borrow_mut();
                    state.is_active = is_visible;
                }
                this.with_callback(
                    |callbacks| &mut callbacks.active_status_change,
                    |callback| callback(is_visible),
                );
            },
        ))
    }

    /// Tracks `fullscreenchange` instead of toggling a local flag: the user
    /// can exit fullscreen with Esc, and `requestFullscreen` can be rejected,
    /// so the document is the only reliable source of truth.
    pub(crate) fn register_fullscreen_change(self: &Rc<Self>) -> Option<EventListenerHandle> {
        let document = self.browser_window.document()?;
        let this = Rc::clone(self);

        Some(EventListenerHandle::add(
            document.as_ref(),
            "fullscreenchange",
            move |_event: JsValue| {
                let is_fullscreen = this
                    .browser_window
                    .document()
                    .is_some_and(|document| document.fullscreen_element().is_some());
                this.state.borrow_mut().is_fullscreen = is_fullscreen;
            },
        ))
    }

    pub(crate) fn with_input_handler<R>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let result = f(&mut handler);
        self.state.borrow_mut().input_handler = Some(handler);
        Some(result)
    }

    pub(crate) fn reset_native_text_input(&self) {
        if !self.uses_native_text_input {
            return;
        }

        self.input_element.set_value("");
        self.input_element.set_selection_range(0, 0).ok();
        *self.native_text_input.borrow_mut() = NativeTextInputState::default();
    }

    fn configure_native_text_input(&self, mode: NativeTextInputMode) {
        let terminal = mode == NativeTextInputMode::Terminal;
        for (attribute, value) in [
            ("autocomplete", "off"),
            ("autocapitalize", "none"),
            ("autocorrect", "off"),
            ("spellcheck", "false"),
        ] {
            if terminal {
                self.input_element.set_attribute(attribute, value).ok();
            } else {
                self.input_element.remove_attribute(attribute).ok();
            }
        }
    }

    pub(crate) fn sync_native_text_input_context(&self) {
        const CONTEXT_UTF16: usize = 1024;

        if !self.uses_native_text_input || self.is_composing.get() {
            return;
        }

        enum InputContext {
            Document {
                value: String,
                document_start_utf16: usize,
                selection_start: usize,
                selection_end: usize,
                reversed: bool,
            },
            Terminal,
        }

        let context = self
            .with_input_handler(|handler| {
                let selection = handler.selected_text_range(true)?;
                let proposed_range = selection.range.start.saturating_sub(CONTEXT_UTF16)
                    ..selection.range.end.saturating_add(CONTEXT_UTF16);
                let mut adjusted_range = None;
                let Some(value) =
                    handler.text_for_range(proposed_range.clone(), &mut adjusted_range)
                else {
                    return Some(InputContext::Terminal);
                };
                let document_range = adjusted_range.unwrap_or(proposed_range);
                if selection.range.start < document_range.start
                    || selection.range.end > document_range.end
                {
                    return None;
                }

                Some(InputContext::Document {
                    value,
                    document_start_utf16: document_range.start,
                    selection_start: selection.range.start - document_range.start,
                    selection_end: selection.range.end - document_range.start,
                    reversed: selection.reversed,
                })
            })
            .flatten();

        let Some(context) = context else {
            self.configure_native_text_input(NativeTextInputMode::Inactive);
            return self.reset_native_text_input();
        };

        if matches!(context, InputContext::Terminal) {
            self.configure_native_text_input(NativeTextInputMode::Terminal);
            self.input_element.set_value("");
            self.input_element.set_selection_range(0, 0).ok();
            *self.native_text_input.borrow_mut() = NativeTextInputState {
                value: String::new(),
                document_start_utf16: None,
                mode: NativeTextInputMode::Terminal,
            };
            return;
        }

        let InputContext::Document {
            value,
            document_start_utf16,
            selection_start,
            selection_end,
            reversed,
        } = context
        else {
            unreachable!()
        };

        let (Ok(selection_start), Ok(selection_end)) =
            (u32::try_from(selection_start), u32::try_from(selection_end))
        else {
            self.configure_native_text_input(NativeTextInputMode::Inactive);
            self.reset_native_text_input();
            return;
        };

        self.configure_native_text_input(NativeTextInputMode::Document);
        self.input_element.set_value(&value);
        self.input_element
            .set_selection_range_with_direction(
                selection_start,
                selection_end,
                if reversed { "backward" } else { "forward" },
            )
            .ok();
        *self.native_text_input.borrow_mut() = NativeTextInputState {
            value,
            document_start_utf16: Some(document_start_utf16),
            mode: NativeTextInputMode::Document,
        };
    }

    pub(crate) fn keyboard_accessory_root(&self) -> Option<web_sys::HtmlElement> {
        self.keyboard_accessory
            .as_ref()
            .map(|accessory| accessory.root.clone())
    }

    pub(crate) fn show_keyboard_accessory(&self) {
        if let Some(accessory) = &self.keyboard_accessory {
            accessory.root.style().set_property("display", "flex").ok();
        }
    }

    pub(crate) fn hide_keyboard_accessory(&self) {
        let Some(accessory) = &self.keyboard_accessory else {
            return;
        };
        accessory.root.style().set_property("display", "none").ok();
        self.keyboard_accessory_expanded.set(false);
        accessory
            .key_row
            .style()
            .set_property("display", "none")
            .ok();
        accessory
            .toggle
            .set_attribute("aria-expanded", "false")
            .ok();
        self.keyboard_accessory_modifiers.set(Modifiers::default());
        update_keyboard_modifier_button(&accessory.control, false);
        update_keyboard_modifier_button(&accessory.alt, false);
    }

    pub(crate) fn toggle_keyboard_accessory(&self) {
        let Some(accessory) = &self.keyboard_accessory else {
            return;
        };
        let expanded = !self.keyboard_accessory_expanded.get();
        self.keyboard_accessory_expanded.set(expanded);
        accessory
            .key_row
            .style()
            .set_property("display", if expanded { "flex" } else { "none" })
            .ok();
        accessory
            .toggle
            .set_attribute("aria-expanded", if expanded { "true" } else { "false" })
            .ok();
    }

    pub(crate) fn toggle_keyboard_accessory_modifier(&self, modifier: &str) {
        let Some(accessory) = &self.keyboard_accessory else {
            return;
        };
        let mut modifiers = self.keyboard_accessory_modifiers.get();
        match modifier {
            "control" => modifiers.control = !modifiers.control,
            "alt" => modifiers.alt = !modifiers.alt,
            _ => return,
        }
        self.keyboard_accessory_modifiers.set(modifiers);
        update_keyboard_modifier_button(&accessory.control, modifiers.control);
        update_keyboard_modifier_button(&accessory.alt, modifiers.alt);
    }

    pub(crate) fn take_keyboard_accessory_modifiers(&self) -> Modifiers {
        let modifiers = self
            .keyboard_accessory_modifiers
            .replace(Modifiers::default());
        if let Some(accessory) = &self.keyboard_accessory {
            update_keyboard_modifier_button(&accessory.control, false);
            update_keyboard_modifier_button(&accessory.alt, false);
        }
        modifiers
    }

    pub(crate) fn update_touch_input_focus(&self, position: Point<Pixels>) {
        // A tap can synchronously move GPUI focus. Draw the invalidated frame now so
        // the platform input handler below represents the control that was tapped.
        self.with_callback(
            |callbacks| &mut callbacks.request_frame,
            |callback| {
                callback(RequestFrameOptions {
                    require_presentation: false,
                    force_render: false,
                })
            },
        );

        let accepts_touch_input = self.soft_keyboard_requested.replace(false)
            || self
                .with_input_handler(|handler| {
                    handler
                        .element_bounds()
                        .is_some_and(|bounds| bounds.contains(&position))
                })
                .unwrap_or(false);

        if accepts_touch_input {
            self.sync_native_text_input_context();
            self.input_element.focus().ok();
            self.show_keyboard_accessory();
        } else {
            self.reset_native_text_input();
            self.input_element.blur().ok();
            self.hide_keyboard_accessory();
            self.update_active_status(true);
        }
    }

    pub(crate) fn register_appearance_change(self: &Rc<Self>) -> Option<EventListenerHandle> {
        let mql = self
            .browser_window
            .match_media("(prefers-color-scheme: dark)")
            .ok()??;

        let this = Rc::clone(self);
        Some(EventListenerHandle::add(
            mql.as_ref(),
            "change",
            move |_event: JsValue| {
                this.with_callback(
                    |callbacks| &mut callbacks.appearance_changed,
                    |callback| callback(),
                );
            },
        ))
    }
}

impl Drop for WebWindow {
    fn drop(&mut self) {
        // Cancel the pending requestAnimationFrame callback before
        // `_raf_closure` is freed, and disconnect the resize observer before
        // `_resize_observer_closure` is freed; a late invocation of either
        // would throw "closure invoked after being dropped".
        if let Some(raf_id) = self.inner.raf_id.take() {
            self.inner
                .browser_window
                .cancel_animation_frame(raf_id)
                .ok();
        }
        if let Some(ref observer) = self._resize_observer {
            observer.disconnect();
        }

        // The DPR media-query closure captures an `Rc<WebWindowInner>` and is
        // stored inside the inner itself, forming a reference cycle; take it
        // out so the inner can actually be freed.
        self.inner.mql_handle.borrow_mut().take();

        let canvas: &web_sys::Element = self.inner.canvas.as_ref();
        canvas.remove();
        let input_element: &web_sys::Element = self.inner.input_element.as_ref();
        input_element.remove();
        if let Some(accessory) = &self.inner.keyboard_accessory {
            let root: &web_sys::Element = accessory.root.as_ref();
            root.remove();
        }
        self.active_window.borrow_mut().take();
        self.lifecycle.set(WebWindowLifecycle::Closed);
    }
}

fn create_mobile_keyboard_accessory(
    document: &web_sys::Document,
    browser_window: &web_sys::Window,
    body: &web_sys::HtmlElement,
) -> anyhow::Result<Option<KeyboardAccessory>> {
    let is_mobile = browser_window
        .match_media("(any-pointer: coarse) and (max-width: 900px)")
        .ok()
        .flatten()
        .is_some_and(|query| query.matches());
    if !is_mobile {
        return Ok(None);
    }

    let is_dark = matches!(current_appearance(browser_window), WindowAppearance::Dark);
    let panel_color = if is_dark { "#24272d" } else { "#f1f2f4" };
    let key_color = if is_dark { "#343840" } else { "#ffffff" };
    let text_color = if is_dark { "#f2f3f5" } else { "#24272d" };
    let border_color = if is_dark { "#515762" } else { "#c7cbd1" };

    let root: web_sys::HtmlElement = document
        .create_element("div")
        .map_err(|error| anyhow::anyhow!("Failed to create keyboard accessory: {error:?}"))?
        .dyn_into()
        .map_err(|error| anyhow::anyhow!("Keyboard accessory is not an HTML element: {error:?}"))?;
    root.set_id("zed-mobile-keyboard-accessory");
    root.set_attribute("role", "toolbar").ok();
    root.set_attribute("aria-label", "Terminal and editor keys")
        .ok();
    set_inline_styles(
        &root,
        &[
            ("position", "fixed"),
            ("right", "8px"),
            ("bottom", "calc(40px + env(safe-area-inset-bottom, 0px))"),
            ("z-index", "2147483647"),
            ("display", "none"),
            ("align-items", "center"),
            ("gap", "4px"),
            ("max-width", "calc(100vw - 16px)"),
            ("height", "40px"),
            ("padding", "2px"),
            ("box-sizing", "border-box"),
            ("background-color", panel_color),
            ("border", &format!("1px solid {border_color}")),
            ("border-radius", "6px"),
            ("box-shadow", "0 2px 8px rgba(0, 0, 0, 0.28)"),
            ("font-family", "system-ui, sans-serif"),
            ("font-size", "13px"),
            ("font-weight", "500"),
            ("letter-spacing", "0"),
            ("touch-action", "manipulation"),
            ("user-select", "none"),
            ("-webkit-user-select", "none"),
            ("-webkit-tap-highlight-color", "transparent"),
        ],
    );

    let key_row: web_sys::HtmlElement = document
        .create_element("div")
        .map_err(|error| anyhow::anyhow!("Failed to create keyboard key row: {error:?}"))?
        .dyn_into()
        .map_err(|error| anyhow::anyhow!("Keyboard key row is not an HTML element: {error:?}"))?;
    key_row.set_id("zed-mobile-keyboard-keys");
    set_inline_styles(
        &key_row,
        &[
            ("display", "none"),
            ("align-items", "center"),
            ("gap", "4px"),
            ("flex", "1 1 auto"),
            ("min-width", "0"),
            ("overflow-x", "auto"),
            ("overscroll-behavior", "contain"),
            ("scrollbar-width", "none"),
        ],
    );

    let escape = create_keyboard_button(
        document,
        "zed-mobile-key-escape",
        "Esc",
        "Escape",
        key_color,
        text_color,
        border_color,
    )?;
    let tab = create_keyboard_button(
        document,
        "zed-mobile-key-tab",
        "Tab",
        "Tab",
        key_color,
        text_color,
        border_color,
    )?;
    let control = create_keyboard_button(
        document,
        "zed-mobile-key-ctrl",
        "Ctrl",
        "Control, applies to the next key",
        key_color,
        text_color,
        border_color,
    )?;
    control.set_attribute("aria-pressed", "false").ok();
    let alt = create_keyboard_button(
        document,
        "zed-mobile-key-alt",
        "Alt",
        "Alt, applies to the next key",
        key_color,
        text_color,
        border_color,
    )?;
    alt.set_attribute("aria-pressed", "false").ok();
    let left = create_keyboard_button(
        document,
        "zed-mobile-key-left",
        "←",
        "Left arrow",
        key_color,
        text_color,
        border_color,
    )?;
    let down = create_keyboard_button(
        document,
        "zed-mobile-key-down",
        "↓",
        "Down arrow",
        key_color,
        text_color,
        border_color,
    )?;
    let up = create_keyboard_button(
        document,
        "zed-mobile-key-up",
        "↑",
        "Up arrow",
        key_color,
        text_color,
        border_color,
    )?;
    let right = create_keyboard_button(
        document,
        "zed-mobile-key-right",
        "→",
        "Right arrow",
        key_color,
        text_color,
        border_color,
    )?;

    for button in [&escape, &tab, &control, &alt, &left, &down, &up, &right] {
        key_row.append_child(button).map_err(|error| {
            anyhow::anyhow!("Failed to append keyboard accessory key: {error:?}")
        })?;
    }

    let toggle = create_keyboard_button(
        document,
        "zed-mobile-keyboard-toggle",
        "⌨",
        "Show special keys",
        key_color,
        text_color,
        border_color,
    )?;
    toggle
        .set_attribute("aria-controls", "zed-mobile-keyboard-keys")
        .ok();
    toggle.set_attribute("aria-expanded", "false").ok();
    toggle.style().set_property("font-size", "18px").ok();

    root.append_child(&key_row)
        .map_err(|error| anyhow::anyhow!("Failed to append keyboard key row: {error:?}"))?;
    root.append_child(&toggle)
        .map_err(|error| anyhow::anyhow!("Failed to append keyboard toggle: {error:?}"))?;
    body.append_child(&root)
        .map_err(|error| anyhow::anyhow!("Failed to append keyboard accessory: {error:?}"))?;

    Ok(Some(KeyboardAccessory {
        root,
        key_row,
        toggle,
        control,
        alt,
    }))
}

fn create_keyboard_button(
    document: &web_sys::Document,
    id: &str,
    label: &str,
    accessible_label: &str,
    background: &str,
    foreground: &str,
    border: &str,
) -> anyhow::Result<web_sys::HtmlElement> {
    let button: web_sys::HtmlElement = document
        .create_element("button")
        .map_err(|error| anyhow::anyhow!("Failed to create keyboard button: {error:?}"))?
        .dyn_into()
        .map_err(|error| anyhow::anyhow!("Keyboard button is not an HTML element: {error:?}"))?;
    button.set_id(id);
    button.set_inner_text(label);
    button.set_attribute("type", "button").ok();
    button.set_attribute("aria-label", accessible_label).ok();
    button.set_attribute("title", accessible_label).ok();
    button
        .set_attribute("data-inactive-background", background)
        .ok();
    set_inline_styles(
        &button,
        &[
            ("flex", "0 0 auto"),
            ("min-width", "34px"),
            ("height", "34px"),
            ("padding", "0 8px"),
            ("box-sizing", "border-box"),
            ("background-color", background),
            ("color", foreground),
            ("border", &format!("1px solid {border}")),
            ("border-radius", "4px"),
            ("font", "inherit"),
            ("letter-spacing", "0"),
            ("line-height", "32px"),
            ("text-align", "center"),
            ("touch-action", "manipulation"),
            ("user-select", "none"),
            ("-webkit-user-select", "none"),
            ("-webkit-tap-highlight-color", "transparent"),
        ],
    );
    Ok(button)
}

fn set_inline_styles(element: &web_sys::HtmlElement, styles: &[(&str, &str)]) {
    for (property, value) in styles {
        element.style().set_property(property, value).ok();
    }
}

fn update_keyboard_modifier_button(button: &web_sys::HtmlElement, active: bool) {
    button
        .set_attribute("aria-pressed", if active { "true" } else { "false" })
        .ok();
    let inactive_background = button.get_attribute("data-inactive-background");
    let background = if active {
        "#477fc2"
    } else {
        inactive_background.as_deref().unwrap_or("transparent")
    };
    button
        .style()
        .set_property("background-color", background)
        .ok();
}

fn current_appearance(browser_window: &web_sys::Window) -> WindowAppearance {
    let is_dark = browser_window
        .match_media("(prefers-color-scheme: dark)")
        .ok()
        .flatten()
        .map(|mql| mql.matches())
        .unwrap_or(false);

    if is_dark {
        WindowAppearance::Dark
    } else {
        WindowAppearance::Light
    }
}

struct MqlHandle {
    mql: web_sys::MediaQueryList,
    _closure: Closure<dyn FnMut(JsValue)>,
}

impl Drop for MqlHandle {
    fn drop(&mut self) {
        self.mql
            .remove_event_listener_with_callback("change", self._closure.as_ref().unchecked_ref())
            .ok();
    }
}

// Safari does not support `devicePixelContentBoxSize`, so detect whether it's available.
fn check_device_pixel_support() -> bool {
    let global: JsValue = js_sys::global().into();
    let Ok(constructor) = js_sys::Reflect::get(&global, &"ResizeObserverEntry".into()) else {
        return false;
    };
    let Ok(prototype) = js_sys::Reflect::get(&constructor, &"prototype".into()) else {
        return false;
    };
    let descriptor = js_sys::Object::get_own_property_descriptor(
        &prototype.unchecked_into::<js_sys::Object>(),
        &"devicePixelContentBoxSize".into(),
    );
    !descriptor.is_undefined()
}

impl raw_window_handle::HasWindowHandle for WebWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let canvas_ref: &JsValue = self.inner.canvas.as_ref();
        let obj = std::ptr::NonNull::from(canvas_ref).cast::<std::ffi::c_void>();
        let handle = raw_window_handle::WebCanvasWindowHandle::new(obj);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl raw_window_handle::HasDisplayHandle for WebWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::web())
    }
}

impl PlatformWindow for WebWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.inner.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.inner.state.borrow().bounds.size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let style = self.inner.canvas.style();
        style
            .set_property("width", &format!("{}px", f32::from(size.width)))
            .ok();
        style
            .set_property("height", &format!("{}px", f32::from(size.height)))
            .ok();
    }

    fn scale_factor(&self) -> f32 {
        self.inner.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        current_appearance(&self.inner.browser_window)
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.inner.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.inner.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.inner.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.inner.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.inner.state.borrow_mut().input_handler.take()
    }

    fn show_soft_keyboard(&self) {
        self.inner.soft_keyboard_requested.set(true);
        self.inner.sync_native_text_input_context();
        self.inner.input_element.focus().ok();
        self.inner.show_keyboard_accessory();
    }

    fn hide_soft_keyboard(&self) {
        self.inner.input_element.blur().ok();
        self.inner.hide_keyboard_accessory();
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        self.inner.state.borrow_mut().is_active = true;
    }

    fn is_active(&self) -> bool {
        self.inner.state.borrow().is_active
    }

    fn is_hovered(&self) -> bool {
        self.inner.state.borrow().is_hovered
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.inner.state.borrow_mut().title = title.to_owned();
        if let Some(document) = self.inner.browser_window.document() {
            document.set_title(title);
        }
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {
        log::warn!("WebWindow::minimize is not supported in the browser");
    }

    fn zoom(&self) {
        log::warn!("WebWindow::zoom is not supported in the browser");
    }

    fn toggle_fullscreen(&self) {
        let Some(document) = self.inner.browser_window.document() else {
            return;
        };

        // `is_fullscreen` is updated by the `fullscreenchange` listener once
        // the transition actually happens (or not, if the request fails).
        if document.fullscreen_element().is_some() {
            document.exit_fullscreen();
        } else {
            let canvas: &web_sys::Element = self.inner.canvas.as_ref();
            canvas.request_fullscreen().ok();
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.inner.state.borrow().is_fullscreen
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.inner.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.inner.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.inner.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.inner.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.inner.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.inner.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        if let Some((width, height)) = self.inner.pending_physical_size.take() {
            if self.inner.canvas.width() != width || self.inner.canvas.height() != height {
                self.inner.canvas.set_width(width);
                self.inner.canvas.set_height(height);
            }

            let mut state = self.inner.state.borrow_mut();
            state.renderer.update_drawable_size(Size {
                width: DevicePixels(width as i32),
                height: DevicePixels(height as i32),
            });
            drop(state);
        }

        self.inner.state.borrow_mut().renderer.draw(scene);
    }

    fn completed_frame(&self) {
        // On web, presentation happens automatically via wgpu surface present
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.inner.state.borrow().renderer.sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.inner
            .state
            .borrow()
            .renderer
            .supports_dual_source_blending()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.inner.state.borrow().renderer.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn request_decorations(&self, _decorations: WindowDecorations) {}

    fn show_window_menu(&self, _position: Point<Pixels>) {}

    fn start_window_move(&self) {}

    fn start_window_resize(&self, _edge: ResizeEdge) {}

    fn window_decorations(&self) -> Decorations {
        Decorations::Server
    }

    fn set_app_id(&mut self, _app_id: &str) {}

    fn window_controls(&self) -> WindowControls {
        WindowControls {
            fullscreen: true,
            maximize: false,
            minimize: false,
            window_menu: false,
        }
    }

    fn set_client_inset(&self, _inset: Pixels) {}
}
