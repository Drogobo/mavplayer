//! The system tray icon shown while "Keep running in the system tray" is on.
//! Clicking it opens the window; its menu opens the window or quits.
//!
//! Linux uses StatusNotifierItem over D-Bus (`ksni`), which KDE, Cinnamon,
//! Xfce, LXQt, Budgie, Sway bars and GNOME's AppIndicator extension show.
//! Windows (notification area) and macOS (menu bar) use `tray-icon`.

use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Open the window (or bring it to the front).
    Show,
    /// Quit MavPlayer completely.
    Quit,
}

type Handler = Arc<dyn Fn(TrayEvent) + Send + Sync>;

/// Removes the icon when dropped.
pub struct Tray {
    _inner: imp::Tray,
}

impl Tray {
    /// Shows the tray icon. Fails when the desktop has no system tray (GNOME
    /// without an extension, say), so closing the window must quit instead.
    /// `on_event` runs on whatever thread the platform delivers clicks on.
    pub fn new(on_event: impl Fn(TrayEvent) + Send + Sync + 'static) -> anyhow::Result<Self> {
        imp::Tray::new(Arc::new(on_event)).map(|inner| Self { _inner: inner })
    }
}

const TOOLTIP: &str = "MavPlayer";
const SHOW_LABEL: &str = "Open MavPlayer";
const QUIT_LABEL: &str = "Quit MavPlayer";

/// The app icon as RGBA pixels at about `size` pixels.
fn icon_rgba(size: u32) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let png: &[u8] = match size {
        0..=16 => include_bytes!("../../packaging/icons/mavplayer-16.png"),
        17..=22 => include_bytes!("../../packaging/icons/mavplayer-22.png"),
        23..=24 => include_bytes!("../../packaging/icons/mavplayer-24.png"),
        25..=32 => include_bytes!("../../packaging/icons/mavplayer-32.png"),
        33..=48 => include_bytes!("../../packaging/icons/mavplayer-48.png"),
        _ => include_bytes!("../../packaging/icons/mavplayer-64.png"),
    };
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgba8();
    let (w, h) = img.dimensions();
    Ok((img.into_raw(), w, h))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Handler, TrayEvent};
    use ksni::blocking::TrayMethods;

    struct Item {
        on_event: Handler,
        icons: Vec<ksni::Icon>,
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            "mavplayer".into()
        }

        fn title(&self) -> String {
            super::TOOLTIP.into()
        }

        // The app's own pictures rather than a themed icon name: a portable
        // build or AppImage has no icon installed in the theme.
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icons.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: super::TOOLTIP.into(), ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            (self.on_event)(TrayEvent::Show);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: super::SHOW_LABEL.into(),
                    icon_name: crate::integration::APP_ID.into(),
                    activate: Box::new(|t: &mut Self| (t.on_event)(TrayEvent::Show)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: super::QUIT_LABEL.into(),
                    icon_name: "application-exit".into(),
                    activate: Box::new(|t: &mut Self| (t.on_event)(TrayEvent::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }

        // The panel went away (restarted, or its tray widget removed). Keep
        // the item: it reappears when the panel comes back, and launching
        // MavPlayer again still reaches this process meanwhile.
        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            log::info!("system tray went away: {reason:?}");
            true
        }
    }

    /// The item's D-Bus name. It stays the same from one process to the next:
    /// the process taking over in the tray requests it with
    /// `ReplaceExisting`, the name passes straight to it, and Plasma (whose
    /// watcher drops an item only once its name has no owner) keeps the icon
    /// as it is. A host that tracks items by unique name (the GNOME
    /// extension) shows the new item next to the old one until that exits.
    /// A separate config folder gets its own name, as it gets its own socket.
    fn bus_name() -> String {
        let base = format!("{}.Tray", crate::integration::APP_ID);
        match std::env::var_os("MAVPLAYER_CONFIG_DIR") {
            Some(d) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                d.hash(&mut h);
                format!("{base}.c{:016x}", h.finish())
            }
            None => base,
        }
    }

    /// ARGB32 in network byte order, as StatusNotifierItem wants.
    fn argb_icon(size: u32) -> Option<ksni::Icon> {
        let (mut px, w, h) = super::icon_rgba(size).ok()?;
        for p in px.as_chunks_mut::<4>().0 {
            p.rotate_right(1); // RGBA → ARGB
        }
        Some(ksni::Icon { width: w as i32, height: h as i32, data: px })
    }

    pub struct Tray(ksni::blocking::Handle<Item>);

    impl Tray {
        pub fn new(on_event: Handler) -> anyhow::Result<Self> {
            let icons = [16, 22, 24, 32, 48, 64].into_iter().filter_map(argb_icon).collect();
            // A Flatpak sandbox may not own the item's usual bus name (it
            // can own `bus_name`, which is under the app ID).
            let sandboxed = std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists();
            let handle = Item { on_event, icons }
                .disable_dbus_name(sandboxed)
                .dbus_name(Some(bus_name()))
                .spawn()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Self(handle))
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.0.shutdown().wait();
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use super::{Handler, TrayEvent};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        _icon: TrayIcon,
    }

    impl Tray {
        /// Must run on the main thread (it owns the platform's event loop).
        pub fn new(on_event: Handler) -> anyhow::Result<Self> {
            let show = MenuItem::new(super::SHOW_LABEL, true, None);
            let quit = MenuItem::new(super::QUIT_LABEL, true, None);
            let menu = Menu::new();
            menu.append_items(&[&show, &PredefinedMenuItem::separator(), &quit])?;
            let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
            let on_menu = on_event.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if e.id == show_id {
                    on_menu(TrayEvent::Show);
                } else if e.id == quit_id {
                    on_menu(TrayEvent::Quit);
                }
            }));
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    on_event(TrayEvent::Show);
                }
            }));
            // Retina menu bars draw the icon at 2x.
            let (rgba, w, h) = super::icon_rgba(if cfg!(target_os = "macos") { 64 } else { 32 })?;
            let icon = TrayIconBuilder::new()
                .with_tooltip(super::TOOLTIP)
                .with_icon(tray_icon::Icon::from_rgba(rgba, w, h)?)
                .with_menu(Box::new(menu))
                // Left click opens the window; the menu is on the right button.
                .with_menu_on_left_click(false)
                .build()?;
            Ok(Self { _icon: icon })
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
            TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        }
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    use super::Handler;

    pub struct Tray;

    impl Tray {
        pub fn new(_: Handler) -> anyhow::Result<Self> {
            anyhow::bail!("no system tray support on this platform")
        }
    }
}
