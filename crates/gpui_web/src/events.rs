use std::rc::Rc;

use gpui::{
    Capslock, ClipboardEntry, ClipboardItem, DispatchEventResult, Image, ImageFormat, KeyDownEvent,
    KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels, PlatformInput,
    Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};
use wasm_bindgen::prelude::*;

use crate::window::WebWindowInner;

pub struct WebEventListeners {
    _handles: Vec<EventListenerHandle>,
}

/// A DOM event listener that is removed from its target when dropped.
///
/// Dropping the `Closure` alone would leave the listener attached to the DOM
/// pointing at a freed function; the next event would then throw "closure
/// invoked after being dropped". Keeping the target alongside the closure
/// lets `Drop` unregister the listener first.
pub(crate) struct EventListenerHandle {
    target: web_sys::EventTarget,
    event_name: &'static str,
    closure: Closure<dyn FnMut(JsValue)>,
}

impl EventListenerHandle {
    pub(crate) fn add(
        target: &web_sys::EventTarget,
        event_name: &'static str,
        handler: impl FnMut(JsValue) + 'static,
    ) -> Self {
        let closure = Closure::<dyn FnMut(JsValue)>::new(handler);
        target
            .add_event_listener_with_callback(event_name, closure.as_ref().unchecked_ref())
            .ok();
        Self {
            target: target.clone(),
            event_name,
            closure,
        }
    }

    /// Registers with `{passive: false}` so that `preventDefault()` works.
    /// Needed for events like `wheel` which are passive by default in modern
    /// browsers. Removal does not need to match the `passive` option, so
    /// `Drop` works the same as for [`EventListenerHandle::add`].
    pub(crate) fn add_non_passive(
        target: &web_sys::EventTarget,
        event_name: &'static str,
        handler: impl FnMut(JsValue) + 'static,
    ) -> Self {
        let closure = Closure::<dyn FnMut(JsValue)>::new(handler);
        let target_js: &JsValue = target.as_ref();
        let callback_js: &JsValue = closure.as_ref();
        let options = js_sys::Object::new();
        js_sys::Reflect::set(&options, &"passive".into(), &false.into()).ok();
        if let Ok(add_fn_val) = js_sys::Reflect::get(target_js, &"addEventListener".into()) {
            if let Ok(add_fn) = add_fn_val.dyn_into::<js_sys::Function>() {
                add_fn
                    .call3(target_js, &event_name.into(), callback_js, &options)
                    .ok();
            }
        }
        Self {
            target: target.clone(),
            event_name,
            closure,
        }
    }
}

impl Drop for EventListenerHandle {
    fn drop(&mut self) {
        self.target
            .remove_event_listener_with_callback(
                self.event_name,
                self.closure.as_ref().unchecked_ref(),
            )
            .ok();
    }
}

pub(crate) struct ClickState {
    last_position: Point<Pixels>,
    last_time: f64,
    current_count: usize,
}

pub(crate) struct TouchPointerState {
    pointer_id: i32,
    start_position: Point<Pixels>,
    last_position: Point<Pixels>,
    scrolling: bool,
}

const TOUCH_SCROLL_THRESHOLD: f32 = 6.0;

impl Default for ClickState {
    fn default() -> Self {
        Self {
            last_position: Point::default(),
            last_time: 0.0,
            current_count: 0,
        }
    }
}

impl ClickState {
    fn register_click(&mut self, position: Point<Pixels>, time: f64) -> usize {
        let distance = ((f32::from(position.x) - f32::from(self.last_position.x)).powi(2)
            + (f32::from(position.y) - f32::from(self.last_position.y)).powi(2))
        .sqrt();

        if (time - self.last_time) < 400.0 && distance < 5.0 {
            self.current_count += 1;
        } else {
            self.current_count = 1;
        }

        self.last_position = position;
        self.last_time = time;
        self.current_count
    }
}

impl WebWindowInner {
    pub fn register_event_listeners(self: &Rc<Self>) -> WebEventListeners {
        let mut handles = vec![
            self.register_pointer_down(),
            self.register_pointer_up(),
            self.register_pointer_cancel(),
            self.register_pointer_move(),
            self.register_pointer_leave(),
            self.register_wheel(),
            self.register_context_menu(),
            self.register_dragover(),
            self.register_drop(),
            self.register_key_down(),
            self.register_key_up(),
            self.register_before_input(),
            self.register_text_input(),
            self.register_paste(),
            self.register_composition_start(),
            self.register_composition_update(),
            self.register_composition_end(),
            self.register_focus(),
            self.register_canvas_focus(),
            self.register_blur(),
            self.register_pointer_enter(),
        ];
        handles.extend(self.register_keyboard_accessory());
        handles.extend(self.register_visibility_change());
        handles.extend(self.register_appearance_change());
        handles.extend(self.register_fullscreen_change());

        WebEventListeners { _handles: handles }
    }

    fn listen(
        self: &Rc<Self>,
        event_name: &'static str,
        handler: impl FnMut(JsValue) + 'static,
    ) -> EventListenerHandle {
        EventListenerHandle::add(self.canvas.as_ref(), event_name, handler)
    }

    fn listen_input(
        self: &Rc<Self>,
        event_name: &'static str,
        handler: impl FnMut(JsValue) + 'static,
    ) -> EventListenerHandle {
        EventListenerHandle::add(self.input_element.as_ref(), event_name, handler)
    }

    fn listen_non_passive(
        self: &Rc<Self>,
        event_name: &'static str,
        handler: impl FnMut(JsValue) + 'static,
    ) -> EventListenerHandle {
        EventListenerHandle::add_non_passive(self.canvas.as_ref(), event_name, handler)
    }

    fn dispatch_input(&self, input: PlatformInput) -> Option<DispatchEventResult> {
        self.with_callback(|callbacks| &mut callbacks.input, |callback| callback(input))
    }

    fn dispatch_accessory_key(&self, key: &str, modifiers: Modifiers) {
        let keystroke = Keystroke {
            modifiers,
            key: key.into(),
            key_char: None,
        };
        self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        }));
        self.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
    }

    fn register_keyboard_accessory(self: &Rc<Self>) -> Option<EventListenerHandle> {
        let root = self.keyboard_accessory_root()?;
        let this = Rc::clone(self);
        Some(EventListenerHandle::add_non_passive(
            root.as_ref(),
            "pointerdown",
            move |event: JsValue| {
                let event: web_sys::PointerEvent = event.unchecked_into();
                event.prevent_default();
                event.stop_propagation();

                let Some(target) = event
                    .target()
                    .and_then(|target| target.dyn_into::<web_sys::HtmlElement>().ok())
                else {
                    return;
                };

                match target.id().as_str() {
                    "zed-mobile-keyboard-toggle" => this.toggle_keyboard_accessory(),
                    "zed-mobile-key-ctrl" => this.toggle_keyboard_accessory_modifier("control"),
                    "zed-mobile-key-alt" => this.toggle_keyboard_accessory_modifier("alt"),
                    "zed-mobile-key-escape" => {
                        let modifiers = this.take_keyboard_accessory_modifiers();
                        this.dispatch_accessory_key("escape", modifiers);
                    }
                    "zed-mobile-key-tab" => {
                        let modifiers = this.take_keyboard_accessory_modifiers();
                        this.dispatch_accessory_key("tab", modifiers);
                    }
                    "zed-mobile-key-left" => {
                        let modifiers = this.take_keyboard_accessory_modifiers();
                        this.dispatch_accessory_key("left", modifiers);
                    }
                    "zed-mobile-key-down" => {
                        let modifiers = this.take_keyboard_accessory_modifiers();
                        this.dispatch_accessory_key("down", modifiers);
                    }
                    "zed-mobile-key-up" => {
                        let modifiers = this.take_keyboard_accessory_modifiers();
                        this.dispatch_accessory_key("up", modifiers);
                    }
                    "zed-mobile-key-right" => {
                        let modifiers = this.take_keyboard_accessory_modifiers();
                        this.dispatch_accessory_key("right", modifiers);
                    }
                    _ => return,
                }

                this.input_element.focus().ok();
            },
        ))
    }

    pub(crate) fn update_active_status(&self, active: bool) {
        let changed = {
            let mut state = self.state.borrow_mut();
            let changed = state.is_active != active;
            state.is_active = active;
            changed
        };
        if changed {
            self.with_callback(
                |callbacks| &mut callbacks.active_status_change,
                |callback| callback(active),
            );
        }
    }

    /// Records the latest modifier state and reports whether it changed, so
    /// that `ModifiersChanged` is only dispatched on actual transitions
    /// rather than for every key event.
    fn update_modifiers(&self, modifiers: Modifiers, capslock: Capslock) -> bool {
        let mut current_state = self.state.borrow_mut();
        let changed = current_state.modifiers != modifiers || current_state.capslock != capslock;
        current_state.modifiers = modifiers;
        current_state.capslock = capslock;
        changed
    }

    fn register_pointer_down(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("pointerdown", move |event: JsValue| {
            let event: web_sys::PointerEvent = event.unchecked_into();
            event.prevent_default();

            // Capture the pointer so drags that leave the canvas keep
            // delivering pointermove/pointerup here; otherwise a release
            // outside the canvas is never seen and `pressed_button` stays
            // stuck. The capture is released implicitly on pointerup.
            this.canvas.set_pointer_capture(event.pointer_id()).ok();

            let button = dom_mouse_button_to_gpui(event.button());
            let position = pointer_position_in_element(&event);
            let modifiers = modifiers_from_mouse_event(&event, this.is_mac);

            if event.pointer_type() == "touch" {
                this.update_active_status(true);
                this.soft_keyboard_requested.set(false);
                this.active_touch.replace(Some(TouchPointerState {
                    pointer_id: event.pointer_id(),
                    start_position: position,
                    last_position: position,
                    scrolling: false,
                }));
                let mut current_state = this.state.borrow_mut();
                current_state.mouse_position = position;
                current_state.modifiers = modifiers;
                return;
            }

            this.input_element.focus().ok();
            let time = js_sys::Date::now();

            this.pressed_button.set(Some(button));
            let click_count = this.click_state.borrow_mut().register_click(position, time);

            {
                let mut current_state = this.state.borrow_mut();
                current_state.mouse_position = position;
                current_state.modifiers = modifiers;
            }

            this.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count,
                first_mouse: false,
            }));
        })
    }

    fn register_pointer_up(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("pointerup", move |event: JsValue| {
            let event: web_sys::PointerEvent = event.unchecked_into();
            event.prevent_default();

            let button = dom_mouse_button_to_gpui(event.button());
            let position = pointer_position_in_element(&event);
            let modifiers = modifiers_from_mouse_event(&event, this.is_mac);

            if event.pointer_type() == "touch" {
                let Some(touch) = this.active_touch.take() else {
                    return;
                };
                if touch.pointer_id != event.pointer_id() {
                    this.active_touch.replace(Some(touch));
                    return;
                }

                {
                    let mut current_state = this.state.borrow_mut();
                    current_state.mouse_position = position;
                    current_state.modifiers = modifiers;
                }

                if touch.scrolling {
                    this.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(0.), px(0.))),
                        modifiers,
                        touch_phase: TouchPhase::Ended,
                    }));
                } else {
                    // Delay the synthetic click until pointerup so a drag can
                    // become a scroll gesture without first selecting text.
                    let button = MouseButton::Left;
                    let click_count = this
                        .click_state
                        .borrow_mut()
                        .register_click(position, js_sys::Date::now());
                    this.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position,
                        modifiers,
                        click_count,
                        first_mouse: false,
                    }));
                    this.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position,
                        modifiers,
                        click_count,
                    }));
                    this.update_touch_input_focus(position);
                }
                return;
            }

            this.pressed_button.set(None);
            let click_count = this.click_state.borrow().current_count;

            {
                let mut current_state = this.state.borrow_mut();
                current_state.mouse_position = position;
                current_state.modifiers = modifiers;
            }

            this.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers,
                click_count,
            }));
        })
    }

    fn register_pointer_cancel(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("pointercancel", move |event: JsValue| {
            let event: web_sys::PointerEvent = event.unchecked_into();
            let Some(touch) = this.active_touch.take() else {
                return;
            };
            if touch.pointer_id != event.pointer_id() {
                this.active_touch.replace(Some(touch));
                return;
            }

            this.canvas.release_pointer_capture(event.pointer_id()).ok();
            if touch.scrolling {
                this.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: touch.last_position,
                    delta: ScrollDelta::Pixels(point(px(0.), px(0.))),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Cancelled,
                }));
            }
        })
    }

    fn register_pointer_move(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("pointermove", move |event: JsValue| {
            let event: web_sys::PointerEvent = event.unchecked_into();
            event.prevent_default();

            let position = pointer_position_in_element(&event);
            let modifiers = modifiers_from_mouse_event(&event, this.is_mac);

            if event.pointer_type() == "touch" {
                let mut active_touch = this.active_touch.borrow_mut();
                let Some(touch) = active_touch.as_mut() else {
                    return;
                };
                if touch.pointer_id != event.pointer_id() {
                    return;
                }

                let total_x = f32::from(position.x - touch.start_position.x);
                let total_y = f32::from(position.y - touch.start_position.y);
                let distance = total_x.hypot(total_y);
                if !touch.scrolling && distance < TOUCH_SCROLL_THRESHOLD {
                    touch.last_position = position;
                    return;
                }

                let touch_phase = if touch.scrolling {
                    TouchPhase::Moved
                } else {
                    touch.scrolling = true;
                    TouchPhase::Started
                };
                let delta = point(
                    position.x - touch.last_position.x,
                    position.y - touch.last_position.y,
                );
                touch.last_position = position;
                drop(active_touch);

                {
                    let mut current_state = this.state.borrow_mut();
                    current_state.mouse_position = position;
                    current_state.modifiers = modifiers;
                }
                this.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(delta),
                    modifiers,
                    touch_phase,
                }));
                return;
            }

            let current_pressed = this.pressed_button.get();

            {
                let mut current_state = this.state.borrow_mut();
                current_state.mouse_position = position;
                current_state.modifiers = modifiers;
            }

            this.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: current_pressed,
                modifiers,
            }));
        })
    }

    fn register_pointer_leave(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("pointerleave", move |event: JsValue| {
            let event: web_sys::PointerEvent = event.unchecked_into();

            let position = pointer_position_in_element(&event);
            let modifiers = modifiers_from_mouse_event(&event, this.is_mac);
            let current_pressed = this.pressed_button.get();

            {
                let mut current_state = this.state.borrow_mut();
                current_state.mouse_position = position;
                current_state.modifiers = modifiers;
                current_state.is_hovered = false;
            }

            this.dispatch_input(PlatformInput::MouseExited(MouseExitEvent {
                position,
                pressed_button: current_pressed,
                modifiers,
            }));

            this.with_callback(
                |callbacks| &mut callbacks.hover_status_change,
                |callback| callback(false),
            );
        })
    }

    fn register_wheel(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_non_passive("wheel", move |event: JsValue| {
            let event: web_sys::WheelEvent = event.unchecked_into();
            event.prevent_default();

            let mouse_event: &web_sys::MouseEvent = event.as_ref();
            let position = mouse_position_in_element(mouse_event);
            let modifiers = modifiers_from_wheel_event(mouse_event, this.is_mac);

            let delta_mode = event.delta_mode();
            let delta = if delta_mode == 1 {
                ScrollDelta::Lines(point(-event.delta_x() as f32, -event.delta_y() as f32))
            } else {
                ScrollDelta::Pixels(point(
                    px(-event.delta_x() as f32),
                    px(-event.delta_y() as f32),
                ))
            };

            {
                let mut current_state = this.state.borrow_mut();
                current_state.modifiers = modifiers;
            }

            this.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta,
                modifiers,
                touch_phase: TouchPhase::Moved,
            }));
        })
    }

    fn register_context_menu(self: &Rc<Self>) -> EventListenerHandle {
        self.listen("contextmenu", move |event: JsValue| {
            let event: web_sys::Event = event.unchecked_into();
            event.prevent_default();
        })
    }

    /// Browsers only expose dropped files as `File` objects, never as
    /// filesystem paths, so no `FileDrop` input can be synthesized: GPUI's
    /// `ExternalPaths` consumers would try to read paths that don't exist.
    /// The events are still intercepted so the browser doesn't navigate to
    /// the dropped file. Delivering actual file drops would require plumbing
    /// `File` contents through a web-specific channel.
    fn register_dragover(self: &Rc<Self>) -> EventListenerHandle {
        self.listen("dragover", move |event: JsValue| {
            let event: web_sys::DragEvent = event.unchecked_into();
            event.prevent_default();
        })
    }

    fn register_drop(self: &Rc<Self>) -> EventListenerHandle {
        self.listen("drop", move |event: JsValue| {
            let event: web_sys::DragEvent = event.unchecked_into();
            event.prevent_default();
        })
    }

    fn register_key_down(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("keydown", move |event: JsValue| {
            let event: web_sys::KeyboardEvent = event.unchecked_into();

            let modifiers = modifiers_from_keyboard_event(&event, this.is_mac);
            let capslock = capslock_from_keyboard_event(&event);

            if this.update_modifiers(modifiers, capslock) {
                this.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers,
                    capslock,
                }));
            }

            let key = dom_key_to_gpui_key(&event);

            if is_modifier_only_key(&key) {
                return;
            }

            // Clipboard reads are asynchronous on the web. Let the browser
            // deliver the synchronous DOM paste event instead of dispatching
            // GPUI's clipboard action for the same shortcut.
            if is_browser_paste_shortcut(&key, &modifiers, this.is_mac) {
                return;
            }

            let is_held = event.repeat();
            let key_char = compute_key_char(&event, &key, &modifiers);

            let keystroke = Keystroke {
                modifiers,
                key: key.clone(),
                key_char: key_char.clone(),
            };

            let result = this.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                is_held,
                prefer_character_input: false,
            }));

            if let Some(result) = result {
                if !result.propagate {
                    event.prevent_default();
                    return;
                }
            }

            if this.is_composing.get() || event.is_composing() {
                event.prevent_default();
                return;
            }

            if matches!(
                key.as_str(),
                "backspace"
                    | "delete"
                    | "left"
                    | "right"
                    | "up"
                    | "down"
                    | "home"
                    | "end"
                    | "pageup"
                    | "pagedown"
                    | "tab"
                    | "enter"
                    | "escape"
            ) {
                event.prevent_default();
            }

            if keystroke_inserts_text(&modifiers, this.is_mac)
                && let Some(text) = key_char
            {
                this.with_input_handler(|handler| {
                    handler.replace_text_in_range(None, &text);
                });
                // The character went into the input handler; suppress browser
                // side-effects for the same keystroke (space scrolling the
                // page, quick-find, etc.). Everything not handled above falls
                // through so browser shortcuts keep their defaults.
                event.prevent_default();
            }
        })
    }

    fn register_key_up(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("keyup", move |event: JsValue| {
            let event: web_sys::KeyboardEvent = event.unchecked_into();

            let modifiers = modifiers_from_keyboard_event(&event, this.is_mac);
            let capslock = capslock_from_keyboard_event(&event);

            if this.update_modifiers(modifiers, capslock) {
                this.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers,
                    capslock,
                }));
            }

            let key = dom_key_to_gpui_key(&event);

            if is_modifier_only_key(&key) {
                return;
            }

            let key_char = compute_key_char(&event, &key, &modifiers);

            let keystroke = Keystroke {
                modifiers,
                key,
                key_char,
            };

            let result = this.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
            if let Some(result) = result {
                if !result.propagate {
                    event.prevent_default();
                }
            }
        })
    }

    fn register_text_input(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("input", move |_event: JsValue| {
            // Mobile virtual keyboards commonly emit `input` without a usable
            // `keydown`. Composition text is committed by `compositionend`.
            if this.is_composing.get() {
                return;
            }

            let text = this.input_element.value();
            if text.is_empty() {
                return;
            }

            let modifiers = this.take_keyboard_accessory_modifiers();
            if modifiers.modified() {
                let Some(character) = text.chars().next() else {
                    return;
                };
                let key = match character {
                    ' ' => "space".to_owned(),
                    character => character.to_lowercase().collect(),
                };
                this.dispatch_accessory_key(&key, modifiers);

                let remainder = &text[character.len_utf8()..];
                if !remainder.is_empty() {
                    this.with_input_handler(|handler| {
                        handler.replace_text_in_range(None, remainder);
                    });
                }
                this.input_element.set_value("");
                return;
            }

            if this
                .with_input_handler(|handler| {
                    handler.replace_text_in_range(None, &text);
                })
                .is_some()
            {
                this.input_element.set_value("");
            }
        })
    }

    fn register_before_input(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("beforeinput", move |event: JsValue| {
            let event: web_sys::InputEvent = event.unchecked_into();
            if this.is_composing.get() || event.is_composing() {
                return;
            }

            let key = match event.input_type().as_str() {
                "deleteContentBackward" | "deleteWordBackward" => "backspace",
                "deleteContentForward" | "deleteWordForward" => "delete",
                "insertLineBreak" | "insertParagraph" => "enter",
                _ => return,
            };

            event.prevent_default();
            let keystroke = Keystroke {
                modifiers: this.take_keyboard_accessory_modifiers(),
                key: key.into(),
                key_char: None,
            };
            this.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }));
            this.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
        })
    }

    /// Paste is delivered through the DOM `paste` event rather than
    /// `Platform::read_from_clipboard`: the browser's asynchronous clipboard
    /// read API cannot fit that synchronous signature, while `ClipboardEvent`
    /// exposes `clipboardData` synchronously inside the event. It fires for
    /// any browser-initiated paste (keyboard, menu bar, context menu).
    fn register_paste(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("paste", move |event: JsValue| {
            let event: web_sys::ClipboardEvent = event.unchecked_into();
            let Some(clipboard_data) = event.clipboard_data() else {
                return;
            };

            let image_files = clipboard_image_files(&clipboard_data);
            if !image_files.is_empty() {
                event.prevent_default();
                let this = Rc::clone(&this);
                wasm_bindgen_futures::spawn_local(async move {
                    let mut entries = Vec::with_capacity(image_files.len());
                    for (file, format) in image_files {
                        match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                            Ok(buffer) => {
                                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                                entries
                                    .push(ClipboardEntry::Image(Image::from_bytes(format, bytes)));
                            }
                            Err(error) => {
                                log::warn!("failed to read pasted browser image: {error:?}");
                            }
                        }
                    }

                    if entries.is_empty() {
                        return;
                    }

                    *this.pending_clipboard.borrow_mut() = Some(ClipboardItem { entries });
                    let modifiers = if this.is_mac {
                        Modifiers::command()
                    } else {
                        Modifiers::control()
                    };
                    this.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: Keystroke {
                            modifiers,
                            key: "v".into(),
                            key_char: None,
                        },
                        is_held: false,
                        prefer_character_input: false,
                    }));
                    this.pending_clipboard.borrow_mut().take();
                });
                return;
            }

            let Ok(text) = clipboard_data.get_data("text/plain") else {
                return;
            };
            if text.is_empty() {
                return;
            }

            event.prevent_default();
            this.with_input_handler(|handler| {
                handler.paste_text(&text);
            });
        })
    }

    fn register_composition_start(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("compositionstart", move |_event: JsValue| {
            this.is_composing.set(true);
        })
    }

    fn register_composition_update(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("compositionupdate", move |event: JsValue| {
            let event: web_sys::CompositionEvent = event.unchecked_into();
            let data = event.data().unwrap_or_default();
            this.is_composing.set(true);
            this.with_input_handler(|handler| {
                handler.replace_and_mark_text_in_range(None, &data, None);
            });
        })
    }

    fn register_composition_end(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("compositionend", move |event: JsValue| {
            let event: web_sys::CompositionEvent = event.unchecked_into();
            let data = event.data().unwrap_or_default();
            this.is_composing.set(false);
            this.with_input_handler(|handler| {
                handler.replace_text_in_range(None, &data);
                handler.unmark_text();
            });
            this.input_element.set_value("");
        })
    }

    fn register_focus(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("focus", move |_event: JsValue| {
            this.update_active_status(true);
            this.show_keyboard_accessory();
        })
    }

    fn register_canvas_focus(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("focus", move |_event: JsValue| {
            this.update_active_status(true);
        })
    }

    fn register_blur(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen_input("blur", move |_event: JsValue| {
            this.update_active_status(false);
            this.hide_keyboard_accessory();
        })
    }

    fn register_pointer_enter(self: &Rc<Self>) -> EventListenerHandle {
        let this = Rc::clone(self);
        self.listen("pointerenter", move |_event: JsValue| {
            {
                let mut state = this.state.borrow_mut();
                state.is_hovered = true;
            }
            this.with_callback(
                |callbacks| &mut callbacks.hover_status_change,
                |callback| callback(true),
            );
        })
    }
}

fn clipboard_image_files(
    clipboard_data: &web_sys::DataTransfer,
) -> Vec<(web_sys::File, ImageFormat)> {
    let items = clipboard_data.items();
    let image_files = (0..items.length())
        .filter_map(|index| {
            let item = items.get(index)?;
            if item.kind() != "file" {
                return None;
            }
            let file = item.get_as_file().ok().flatten()?;
            let format = ImageFormat::from_mime_type(&item.type_())
                .or_else(|| ImageFormat::from_mime_type(&file.type_()))?;
            Some((file, format))
        })
        .collect::<Vec<_>>();

    if !image_files.is_empty() {
        return image_files;
    }

    // Some browsers expose pasted screenshots only through `files`, or leave
    // the DataTransferItem MIME type empty while retaining it on the File.
    let Some(files) = clipboard_data.files() else {
        return Vec::new();
    };
    (0..files.length())
        .filter_map(|index| {
            let file = files.item(index)?;
            let format = ImageFormat::from_mime_type(&file.type_())?;
            Some((file, format))
        })
        .collect()
}

fn dom_key_to_gpui_key(event: &web_sys::KeyboardEvent) -> String {
    let key = event.key();
    match key.as_str() {
        "Enter" => "enter".to_string(),
        "Backspace" => "backspace".to_string(),
        "Tab" => "tab".to_string(),
        "Escape" => "escape".to_string(),
        "Delete" => "delete".to_string(),
        " " => "space".to_string(),
        "ArrowLeft" => "left".to_string(),
        "ArrowRight" => "right".to_string(),
        "ArrowUp" => "up".to_string(),
        "ArrowDown" => "down".to_string(),
        "Home" => "home".to_string(),
        "End" => "end".to_string(),
        "PageUp" => "pageup".to_string(),
        "PageDown" => "pagedown".to_string(),
        "Insert" => "insert".to_string(),
        "Control" => "control".to_string(),
        "Alt" => "alt".to_string(),
        "Shift" => "shift".to_string(),
        "Meta" => "platform".to_string(),
        "CapsLock" => "capslock".to_string(),
        other => {
            if let Some(rest) = other.strip_prefix('F') {
                if let Ok(number) = rest.parse::<u8>() {
                    if (1..=35).contains(&number) {
                        return format!("f{number}");
                    }
                }
            }
            other.to_lowercase()
        }
    }
}

fn dom_mouse_button_to_gpui(button: i16) -> MouseButton {
    match button {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        3 => MouseButton::Navigate(NavigationDirection::Back),
        4 => MouseButton::Navigate(NavigationDirection::Forward),
        _ => MouseButton::Left,
    }
}

fn modifiers_from_keyboard_event(event: &web_sys::KeyboardEvent, _is_mac: bool) -> Modifiers {
    Modifiers {
        control: event.ctrl_key(),
        alt: event.alt_key(),
        shift: event.shift_key(),
        platform: event.meta_key(),
        function: false,
    }
}

fn modifiers_from_mouse_event(event: &web_sys::PointerEvent, _is_mac: bool) -> Modifiers {
    let mouse_event: &web_sys::MouseEvent = event.as_ref();
    Modifiers {
        control: mouse_event.ctrl_key(),
        alt: mouse_event.alt_key(),
        shift: mouse_event.shift_key(),
        platform: mouse_event.meta_key(),
        function: false,
    }
}

fn modifiers_from_wheel_event(event: &web_sys::MouseEvent, _is_mac: bool) -> Modifiers {
    Modifiers {
        control: event.ctrl_key(),
        alt: event.alt_key(),
        shift: event.shift_key(),
        platform: event.meta_key(),
        function: false,
    }
}

fn capslock_from_keyboard_event(event: &web_sys::KeyboardEvent) -> Capslock {
    Capslock {
        on: event.get_modifier_state("CapsLock"),
    }
}

pub(crate) fn browser_operating_system(browser_window: &web_sys::Window) -> gpui::OperatingSystem {
    let navigator = browser_window.navigator();

    #[allow(deprecated)]
    // navigator.platform() is deprecated but navigator.userAgentData is not widely available yet
    if let Ok(platform) = navigator.platform() {
        if platform.contains("Mac") || platform.contains("iPhone") || platform.contains("iPad") {
            return gpui::OperatingSystem::Mac;
        }
        if platform.contains("Win") {
            return gpui::OperatingSystem::Windows;
        }
        if platform.contains("Linux") {
            return gpui::OperatingSystem::Linux;
        }
    }

    if let Ok(user_agent) = navigator.user_agent() {
        if user_agent.contains("Mac")
            || user_agent.contains("iPhone")
            || user_agent.contains("iPad")
        {
            return gpui::OperatingSystem::Mac;
        }
        if user_agent.contains("Windows") {
            return gpui::OperatingSystem::Windows;
        }
        if user_agent.contains("Linux") || user_agent.contains("X11") {
            return gpui::OperatingSystem::Linux;
        }
    }

    gpui::OperatingSystem::Unknown
}

pub(crate) fn is_mac_platform(browser_window: &web_sys::Window) -> bool {
    browser_operating_system(browser_window) == gpui::OperatingSystem::Mac
}

fn is_modifier_only_key(key: &str) -> bool {
    matches!(
        key,
        "control" | "alt" | "shift" | "platform" | "capslock" | "compose" | "process"
    )
}

fn is_browser_paste_shortcut(key: &str, modifiers: &Modifiers, is_mac: bool) -> bool {
    if key != "v" || modifiers.alt || modifiers.function {
        return false;
    }

    if is_mac {
        modifiers.platform && !modifiers.control
    } else {
        modifiers.control && !modifiers.platform
    }
}

/// Whether a keystroke with these modifiers produces text to insert.
///
/// On macOS, Option participates in text entry (e.g. option-n composes "~"
/// or accented characters), so only Command and Control disqualify. Elsewhere,
/// plain Alt is a shortcut modifier, but AltGr is reported by browsers as
/// control+alt and `event.key()` then carries the composed character.
fn keystroke_inserts_text(modifiers: &Modifiers, is_mac: bool) -> bool {
    if is_mac {
        !modifiers.platform && !modifiers.control
    } else {
        modifiers.is_subset_of(&Modifiers::shift()) || (modifiers.control && modifiers.alt)
    }
}

fn compute_key_char(
    event: &web_sys::KeyboardEvent,
    gpui_key: &str,
    modifiers: &Modifiers,
) -> Option<String> {
    // AltGr arrives as control+alt with the composed character in
    // `event.key()`; bare Command/Control combinations are not text.
    if (modifiers.platform || modifiers.control) && !(modifiers.control && modifiers.alt) {
        return None;
    }

    if is_modifier_only_key(gpui_key) {
        return None;
    }

    if gpui_key == "space" {
        return Some(" ".to_string());
    }

    let raw_key = event.key();

    if raw_key.len() == 1 {
        return Some(raw_key);
    }

    None
}

fn pointer_position_in_element(event: &web_sys::PointerEvent) -> Point<Pixels> {
    let mouse_event: &web_sys::MouseEvent = event.as_ref();
    mouse_position_in_element(mouse_event)
}

fn mouse_position_in_element(event: &web_sys::MouseEvent) -> Point<Pixels> {
    // offset_x/offset_y give position relative to the target element's padding edge
    point(px(event.offset_x() as f32), px(event.offset_y() as f32))
}
