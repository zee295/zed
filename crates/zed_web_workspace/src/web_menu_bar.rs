//! In-window application menu bar for the browser (desktop-style File/Edit/View…).
//!
//! Mirrors `title_bar::application_menu::ApplicationMenu` without pulling the
//! full `title_bar` crate (collab / livekit / etc. don't compile for wasm).

use gpui::{
    App, Context, Entity, IntoElement, OwnedMenu, OwnedMenuItem, ParentElement, Render,
    SharedString, Window, div,
};
use smallvec::SmallVec;
use ui::{
    Button, ButtonStyle, ContextMenu, IconButton, IconSize, LabelSize, PopoverMenu,
    PopoverMenuHandle, Tooltip, prelude::*,
};

#[derive(Clone)]
struct MenuEntry {
    menu: OwnedMenu,
    handle: PopoverMenuHandle<ContextMenu>,
}

/// Horizontal menu bar: File · Edit · Selection · View · Go · …
pub struct WebMenuBar {
    entries: SmallVec<[MenuEntry; 8]>,
    compact_handle: PopoverMenuHandle<ContextMenu>,
}

impl WebMenuBar {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let menus = cx.get_menus().unwrap_or_default();
        Self {
            entries: menus
                .into_iter()
                .map(|menu| MenuEntry {
                    menu,
                    handle: PopoverMenuHandle::default(),
                })
                .collect(),
            compact_handle: PopoverMenuHandle::default(),
        }
    }

    fn sanitize_menu_items(items: Vec<OwnedMenuItem>) -> Vec<OwnedMenuItem> {
        let mut cleaned = Vec::new();
        let mut last_was_separator = false;

        for item in items {
            match item {
                OwnedMenuItem::Separator => {
                    if !last_was_separator {
                        cleaned.push(item);
                        last_was_separator = true;
                    }
                }
                OwnedMenuItem::Submenu(submenu) => {
                    if !submenu.items.is_empty() {
                        cleaned.push(OwnedMenuItem::Submenu(submenu));
                        last_was_separator = false;
                    }
                }
                item => {
                    cleaned.push(item);
                    last_was_separator = false;
                }
            }
        }

        if let Some(OwnedMenuItem::Separator) = cleaned.last() {
            cleaned.pop();
        }

        cleaned
    }

    fn build_menu_from_items(
        entry: MenuEntry,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, |menu, window, cx| {
            let menu = menu.when_some(window.focused(cx), |menu, focused| menu.context(focused));
            Self::populate_menu(menu, entry.menu.items)
        })
    }

    fn populate_menu(menu: ContextMenu, items: Vec<OwnedMenuItem>) -> ContextMenu {
        Self::sanitize_menu_items(items)
            .into_iter()
            .fold(menu, |menu, item| match item {
                OwnedMenuItem::Separator => menu.separator(),
                OwnedMenuItem::Action {
                    name,
                    action,
                    checked,
                    disabled,
                    ..
                } => menu.action_checked_with_disabled(name, action, checked, disabled),
                OwnedMenuItem::Submenu(submenu) => Self::populate_menu(menu, submenu.items),
                OwnedMenuItem::SystemMenu(_) => menu,
            })
    }

    fn render_compact_menu(&self) -> impl IntoElement {
        let entries = self.entries.clone();

        PopoverMenu::new("compact-application-menu")
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, _| {
                    entries.iter().fold(menu, |menu, entry| {
                        let entry = entry.clone();
                        menu.submenu(entry.menu.name.clone(), move |menu, _, _| {
                            Self::populate_menu(menu, entry.menu.items.clone())
                        })
                    })
                }))
            })
            .trigger_with_tooltip(
                IconButton::new("compact-application-menu-trigger", IconName::Menu)
                    .style(ButtonStyle::Subtle)
                    .icon_size(IconSize::Small)
                    .tab_index(0isize)
                    .aria_label("Application menu"),
                Tooltip::text("Application menu"),
            )
            .with_handle(self.compact_handle.clone())
    }

    fn render_standard_menu(&self, entry: &MenuEntry) -> impl IntoElement {
        let current_handle = entry.handle.clone();
        let menu_name = entry.menu.name.clone();
        let entry = entry.clone();

        let all_handles: Vec<_> = self
            .entries
            .iter()
            .map(|entry| entry.handle.clone())
            .collect();

        div()
            .id(SharedString::from(format!("{}-menu-item", menu_name)))
            .occlude()
            .child(
                PopoverMenu::new(format!("{}-menu-popover", menu_name))
                    .menu(move |window, cx| {
                        Self::build_menu_from_items(entry.clone(), window, cx).into()
                    })
                    .trigger(
                        Button::new(
                            SharedString::from(format!("{}-menu-trigger", menu_name)),
                            menu_name,
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .tab_index(0isize),
                    )
                    .with_handle(current_handle.clone()),
            )
            .on_hover(move |hover_enter, window, cx| {
                // Desktop-style: hover opens adjacent menus while one is open.
                if *hover_enter && !current_handle.is_deployed() {
                    let any_open = all_handles.iter().any(|h| h.is_deployed());
                    if any_open {
                        all_handles.iter().for_each(|h| h.hide(cx));
                        let handle = current_handle.clone();
                        window.defer(cx, move |window, cx| handle.show(window, cx));
                    }
                }
            })
    }
}

impl Render for WebMenuBar {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let compact = window.viewport_size().width <= px(600.);

        div()
            .key_context("ApplicationMenu")
            .flex()
            .flex_row()
            .items_center()
            .gap_x_0p5()
            .when(compact, |menu| menu.child(self.render_compact_menu()))
            .when(!compact, |menu| {
                menu.children(
                    self.entries
                        .iter()
                        .map(|entry| self.render_standard_menu(entry)),
                )
            })
    }
}
