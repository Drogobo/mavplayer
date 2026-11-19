//! In-window popup menus (menu bar menus, context menu, dropdowns).

use super::player::PlayerView;
use super::theme::Theme;
use super::widgets::icon;
use crate::icons::Icon;
use gpui::{
    AnyElement, Bounds, Context, Corner, MouseButton, Pixels, Point, SharedString, Window, anchored, canvas, deferred,
    div, point, prelude::*, px,
};
use std::cell::RefCell;
use std::rc::Rc;

pub type MenuFn = Rc<dyn Fn(&mut PlayerView, &mut Window, &mut Context<PlayerView>)>;

#[derive(Clone)]
pub enum MenuItem {
    Action {
        label: SharedString,
        shortcut: Option<SharedString>,
        checked: Option<bool>,
        radio: bool,
        enabled: bool,
        icon: Option<Icon>,
        run: MenuFn,
    },
    Submenu {
        label: SharedString,
        icon: Option<Icon>,
        enabled: bool,
        items: Vec<MenuItem>,
    },
    Header(SharedString),
    Separator,
}

impl MenuItem {
    pub fn action(
        label: impl Into<SharedString>,
        run: impl Fn(&mut PlayerView, &mut Window, &mut Context<PlayerView>) + 'static,
    ) -> Self {
        MenuItem::Action {
            label: label.into(),
            shortcut: None,
            checked: None,
            radio: false,
            enabled: true,
            icon: None,
            run: Rc::new(run),
        }
    }
    pub fn submenu(label: impl Into<SharedString>, items: Vec<MenuItem>) -> Self {
        let enabled = !items.is_empty();
        MenuItem::Submenu { label: label.into(), icon: None, enabled, items }
    }
    pub fn shortcut(mut self, keys: &str) -> Self {
        if let MenuItem::Action { shortcut, .. } = &mut self {
            *shortcut = Some(super::actions::shortcut(keys).into());
        }
        self
    }
    pub fn checked(mut self, on: bool) -> Self {
        if let MenuItem::Action { checked, .. } = &mut self {
            *checked = Some(on);
        }
        self
    }
    /// Like `checked` but drawn as a radio choice.
    pub fn selected(mut self, on: bool) -> Self {
        if let MenuItem::Action { checked, radio, .. } = &mut self {
            *checked = Some(on);
            *radio = true;
        }
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        match &mut self {
            MenuItem::Action { enabled, .. } | MenuItem::Submenu { enabled, .. } => *enabled = on,
            _ => {}
        }
        self
    }
    pub fn icon(mut self, i: Icon) -> Self {
        match &mut self {
            MenuItem::Action { icon, .. } | MenuItem::Submenu { icon, .. } => *icon = Some(i),
            _ => {}
        }
        self
    }
}

pub struct OpenMenu {
    pub items: Vec<MenuItem>,
    pub pos: Point<Pixels>,
    /// Which submenu is open at each nesting level.
    pub open_path: Vec<usize>,
    /// Index in the menu bar this menu belongs to.
    pub menubar_index: Option<usize>,
    pub min_width: Pixels,
    /// Measured on-screen bounds of the open submenu row at each level, so
    /// submenu panels can be anchored next to it (GPUI can't nest `deferred`).
    pub anchors: Rc<RefCell<Vec<Bounds<Pixels>>>>,
}

const ITEM_H: f32 = 28.0;
const MENU_W: f32 = 250.0;

impl PlayerView {
    pub fn open_menu(
        &mut self,
        items: Vec<MenuItem>,
        pos: Point<Pixels>,
        menubar_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(OpenMenu {
            items,
            pos,
            open_path: vec![],
            menubar_index,
            min_width: px(MENU_W),
            anchors: Default::default(),
        });
        cx.notify();
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    pub fn render_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let theme = self.theme;
        let win_w = window.viewport_size().width;
        let open_left = menu.pos.x + px(MENU_W * 2.0) > win_w;
        let mut layer = div().absolute().top_0().left_0().size_full().child(
            div()
                .id("menu-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.close_menu(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.close_menu(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation())),
        );
        layer = layer.child(anchored().position(menu.pos).snap_to_window_with_margin(px(4.)).child(self.menu_panel(
            &menu.items,
            0,
            &menu.open_path,
            theme,
            menu.min_width,
            &menu.anchors,
            cx,
        )));
        // Open submenus, each anchored beside the row that opened it.
        let anchors = menu.anchors.borrow().clone();
        let mut items = &menu.items;
        for (level, &ix) in menu.open_path.iter().enumerate() {
            let Some(MenuItem::Submenu { items: sub, enabled: true, .. }) = items.get(ix) else { break };
            let Some(row) = anchors.get(level).copied().filter(|b| b.size.width > px(0.)) else { break };
            let (pos, corner) = if open_left {
                (point(row.left() - px(4.), row.top() - px(5.)), Corner::TopRight)
            } else {
                (point(row.right() + px(4.), row.top() - px(5.)), Corner::TopLeft)
            };
            layer = layer.child(
                anchored().position(pos).anchor(corner).snap_to_window_with_margin(px(4.)).child(self.menu_panel(
                    sub,
                    level + 1,
                    &menu.open_path,
                    theme,
                    px(200.),
                    &menu.anchors,
                    cx,
                )),
            );
            items = sub;
        }
        Some(deferred(layer).with_priority(10).into_any_element())
    }

    #[allow(clippy::too_many_arguments)]
    fn menu_panel(
        &self,
        items: &[MenuItem],
        level: usize,
        open_path: &[usize],
        theme: Theme,
        min_width: Pixels,
        anchors: &Rc<RefCell<Vec<Bounds<Pixels>>>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_checks = items.iter().any(|i| {
            matches!(i, MenuItem::Action { checked: Some(_), .. })
                || matches!(i, MenuItem::Action { icon: Some(_), .. })
                || matches!(i, MenuItem::Submenu { icon: Some(_), .. })
        });
        let mut panel = div()
            .id(SharedString::from(format!("menu-panel-{level}")))
            .flex()
            .flex_col()
            .min_w(min_width)
            .max_h(px(560.))
            .overflow_y_scroll()
            .py_1()
            .bg(theme.popup)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.))
            .shadow_lg()
            .text_size(px(13.))
            .text_color(theme.text)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        for (i, item) in items.iter().enumerate() {
            let open_here = open_path.get(level) == Some(&i);
            let path_prefix: Vec<usize> = open_path.iter().take(level).copied().collect();
            let el: AnyElement = match item {
                MenuItem::Separator => div().h(px(1.)).my_1().mx_2().bg(theme.border).into_any_element(),
                MenuItem::Header(t) => div()
                    .px_3()
                    .pt_1()
                    .pb(px(2.))
                    .text_size(px(11.))
                    .text_color(theme.text_dim)
                    .child(t.clone())
                    .into_any_element(),
                MenuItem::Action { label, shortcut, checked, radio, enabled, icon: ic, run } => {
                    let run = run.clone();
                    let enabled = *enabled;
                    let color = if enabled { theme.text } else { theme.text_disabled };
                    let lead: AnyElement = match (checked, ic) {
                        (Some(true), _) if *radio => div()
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(px(7.)).rounded_full().bg(theme.accent))
                            .into_any_element(),
                        (Some(true), _) => icon(Icon::Check, px(14.), theme.accent).into_any_element(),
                        (_, Some(ic)) => icon(*ic, px(14.), color).into_any_element(),
                        _ => div().size(px(14.)).into_any_element(),
                    };
                    let pp = path_prefix.clone();
                    div()
                        .id(SharedString::from(format!("mi-{level}-{i}")))
                        .h(px(ITEM_H))
                        .mx_1()
                        .px_2()
                        .gap_2()
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .text_color(color)
                        .when(enabled, |d| d.hover(|s| s.bg(theme.accent).text_color(theme.on_accent)))
                        .when(has_checks, |d| d.child(lead))
                        .child(div().flex_1().truncate().child(label.clone()))
                        .when_some(shortcut.clone(), |d, sc| d.child(div().text_size(px(11.)).opacity(0.65).child(sc)))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered
                                && let Some(m) = this.menu.as_mut()
                                && m.open_path.len() > pp.len()
                            {
                                m.open_path.truncate(pp.len());
                                cx.notify();
                            }
                        }))
                        .when(enabled, |d| {
                            d.on_click(cx.listener(move |this, _, window, cx| {
                                this.menu = None;
                                run(this, window, cx);
                                cx.notify();
                            }))
                        })
                        .into_any_element()
                }
                MenuItem::Submenu { label, icon: ic, enabled, .. } => {
                    let enabled = *enabled;
                    let color = if enabled { theme.text } else { theme.text_disabled };
                    let mut pp = path_prefix.clone();
                    pp.push(i);
                    let pp2 = pp.clone();
                    let mut row = div()
                        .id(SharedString::from(format!("ms-{level}-{i}")))
                        .relative()
                        .h(px(ITEM_H))
                        .mx_1()
                        .px_2()
                        .gap_2()
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .text_color(color)
                        .when(open_here, |d| d.bg(theme.hover))
                        .when(enabled, |d| d.hover(|s| s.bg(theme.hover)))
                        .when(has_checks, |d| {
                            d.child(match ic {
                                Some(ic) => icon(*ic, px(14.), color).into_any_element(),
                                None => div().size(px(14.)).into_any_element(),
                            })
                        })
                        .child(div().flex_1().truncate().child(label.clone()))
                        .child(icon(Icon::ChevronRight, px(12.), theme.text_dim));
                    if enabled {
                        row = row
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered
                                    && let Some(m) = this.menu.as_mut()
                                    && m.open_path != pp
                                {
                                    m.open_path = pp.clone();
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(m) = this.menu.as_mut() {
                                    m.open_path = pp2.clone();
                                    cx.notify();
                                }
                            }));
                    }
                    if open_here && enabled {
                        // Record where this row is so the submenu can be placed next to it.
                        let anchors = anchors.clone();
                        row = row.child(
                            canvas(
                                move |b, window, _| {
                                    let mut a = anchors.borrow_mut();
                                    if a.len() <= level {
                                        a.resize(level + 1, Bounds::default());
                                    }
                                    if a[level] != b {
                                        a[level] = b;
                                        window.request_animation_frame();
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        );
                    }
                    row.into_any_element()
                }
            };
            panel = panel.child(el);
        }
        panel.into_any_element()
    }
}

pub fn below(bounds: gpui::Bounds<Pixels>) -> Point<Pixels> {
    point(bounds.left(), bounds.bottom() + px(2.))
}
