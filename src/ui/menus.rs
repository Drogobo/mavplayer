//! Menu contents (menu bar, context menu, control-bar popups).

use super::actions::*;
use super::dialogs::Dialog;
use super::menu::MenuItem as M;
use super::player::{ASPECTS, PlayerView, SPEEDS};
use super::widgets::format_time;
use crate::engine::{ChannelMode, Deinterlace, HwDecode, MediaSource, Rotation, TrackKind};
use crate::icons::Icon;
use crate::settings::{RepeatMode, SubStyle, ThemeMode};
use gpui::{Action, Context};

pub const MENUBAR: &[&str] = &["Media", "Playback", "Audio", "Video", "Subtitle", "View", "Help"];

fn act<A: Action + Clone>(label: &str, a: A) -> M {
    M::action(label.to_string(), move |_, w, cx| w.dispatch_action(a.boxed_clone(), cx))
}

impl PlayerView {
    pub fn menubar_menu(&self, i: usize, cx: &mut Context<Self>) -> Vec<M> {
        match i {
            0 => self.menu_media(cx),
            1 => self.menu_playback(),
            2 => self.menu_audio(),
            3 => self.menu_video(),
            4 => self.menu_subtitle(),
            5 => self.menu_view(),
            _ => self.menu_help(),
        }
    }

    pub fn context_menu(&self, cx: &mut Context<Self>) -> Vec<M> {
        let playing = self.player.is_active();
        let paused = self.player.paused();
        vec![
            act(if paused || !playing { "Play" } else { "Pause" }, TogglePause)
                .shortcut("space")
                .icon(if paused || !playing { Icon::Play } else { Icon::Pause })
                .enabled(playing),
            act("Stop", Stop).icon(Icon::Stop).enabled(playing),
            act("Previous", PlayPrevious).icon(Icon::Previous).enabled(!self.playlist.items.is_empty()),
            act("Next", PlayNext).icon(Icon::Next).enabled(!self.playlist.items.is_empty()),
            M::Separator,
            M::submenu("Media", self.menu_media(cx)),
            M::submenu("Playback", self.menu_playback()),
            M::submenu("Audio", self.menu_audio()),
            M::submenu("Video", self.menu_video()),
            M::submenu("Subtitle", self.menu_subtitle()),
            M::submenu("View", self.menu_view()),
            M::Separator,
            act("Fullscreen", ToggleFullscreen).shortcut("f").icon(Icon::Fullscreen),
            act("Media Information", ShowMediaInfo).shortcut("i").icon(Icon::Info).enabled(playing),
            act("Preferences…", ShowPreferences).shortcut("secondary-,").icon(Icon::Settings),
        ]
    }

    fn menu_media(&self, _cx: &mut Context<Self>) -> Vec<M> {
        let mut recent: Vec<M> = self
            .settings
            .recent
            .iter()
            .map(|r| {
                let src = r.clone();
                M::action(src.display_name(), move |this, w, cx| this.open_sources(vec![src.clone()], w, cx))
            })
            .collect();
        if !recent.is_empty() {
            recent.push(M::Separator);
            recent.push(M::action("Clear Recent", |this, _, _| {
                this.settings.recent.clear();
                this.settings.save();
            }));
        }
        let mut v = vec![
            act("Open File…", OpenFile).shortcut("secondary-o").icon(Icon::OpenFile),
            act("Open Folder…", OpenFolder).shortcut("secondary-f").icon(Icon::OpenFolder),
            act("Open Disc…", OpenDisc).shortcut("secondary-d").icon(Icon::Disc),
            act("Open URL…", OpenUrl).shortcut("secondary-u").icon(Icon::Url),
            M::submenu("Recent Media", recent).icon(Icon::Recent),
            M::Separator,
            act("Load Subtitle File…", OpenSubtitle)
                .shortcut("secondary-shift-o")
                .icon(Icon::Subtitles)
                .enabled(self.player.is_active()),
            act("Save Playlist…", SavePlaylist)
                .shortcut("secondary-s")
                .icon(Icon::Save)
                .enabled(!self.playlist.items.is_empty()),
        ];
        #[cfg(target_os = "linux")]
        if crate::integration::is_appimage() {
            v.push(M::Separator);
            if crate::integration::is_integrated() {
                v.push(M::action("Remove Desktop Integration", |this, _, cx| {
                    match crate::integration::unintegrate() {
                        Ok(()) => this.osd("Removed from the application menu", cx),
                        Err(e) => this.osd(format!("Could not remove integration: {e}"), cx),
                    }
                }));
            } else {
                v.push(M::action("Integrate into Desktop…", |this, w, cx| {
                    this.show_dialog(Dialog::Integrate, w, cx)
                }));
            }
        }
        v.push(M::Separator);
        v.push(act("Quit", Quit).shortcut("secondary-q"));
        v
    }

    fn menu_playback(&self) -> Vec<M> {
        let active = self.player.is_active();
        let st = self.player.state();
        let speed = self.player.speed();
        let speeds: Vec<M> = SPEEDS
            .iter()
            .map(|&s| {
                M::action(format!("{s}×"), move |this, _, cx| this.set_speed(s, cx)).selected((speed - s).abs() < 1e-3)
            })
            .collect();
        let chapters: Vec<M> = st
            .media
            .as_ref()
            .map(|m| {
                let pos = self.player.position();
                m.chapters
                    .iter()
                    .map(|c| {
                        let start = c.start;
                        let cur = pos >= c.start && pos < c.end;
                        M::action(format!("{}  ({})", c.title, format_time(c.start)), move |this, _, _| {
                            this.player.seek(start, false, true)
                        })
                        .selected(cur)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut v = vec![
            act(if self.player.paused() || !active { "Play" } else { "Pause" }, TogglePause)
                .shortcut("space")
                .enabled(active),
            act("Stop", Stop).shortcut("secondary-.").enabled(active),
            act("Previous", PlayPrevious).shortcut("shift-n"),
            act("Next", PlayNext).shortcut("n"),
            M::Separator,
            act("Jump Forward", SeekForward).shortcut("right").enabled(active),
            act("Jump Backward", SeekBackward).shortcut("left").enabled(active),
            act("Jump to Time…", JumpToTime).shortcut("secondary-t").enabled(active),
            M::submenu("Chapters", chapters),
        ];
        if let Some((path, titles)) = &self.dvd_titles {
            let cur = match &st.source {
                Some(MediaSource::Dvd { title, .. }) => *title,
                _ => 0,
            };
            let items: Vec<M> = titles
                .iter()
                .map(|t| {
                    let (p, n) = (path.clone(), t.number);
                    M::action(
                        format!("Title {}  ({}, {} chapters)", t.number, format_time(t.duration), t.chapters),
                        move |this, w, cx| this.play_dvd_title(p.clone(), n, w, cx),
                    )
                    .selected(cur == t.number)
                })
                .collect();
            v.push(M::submenu("DVD Titles", items).icon(Icon::Disc));
        }
        let rep = self.playlist.repeat;
        v.extend([
            M::Separator,
            M::submenu("Speed", speeds).icon(Icon::Speed),
            act("Faster", SpeedUp).shortcut("]"),
            act("Slower", SpeedDown).shortcut("["),
            act("Normal Speed", SpeedReset).shortcut("backspace"),
            M::Separator,
            act("A-B Loop", ToggleAbLoop).shortcut("a").checked(self.ab_loop.0.is_some()).enabled(active),
            act("Loop File", ToggleLoopFile).shortcut("shift-l").checked(self.loop_file),
            M::submenu(
                "Repeat",
                vec![
                    M::action("Off", |this, _, _| this.playlist.repeat = RepeatMode::Off)
                        .selected(rep == RepeatMode::Off),
                    M::action("Repeat All", |this, _, _| this.playlist.repeat = RepeatMode::All)
                        .selected(rep == RepeatMode::All),
                    M::action("Repeat Current", |this, _, _| this.playlist.repeat = RepeatMode::One)
                        .selected(rep == RepeatMode::One),
                ],
            )
            .icon(Icon::RepeatAll),
            act("Shuffle", ToggleShuffle).shortcut("shift-s").checked(self.playlist.shuffle()),
            M::Separator,
            act("Next Frame", FrameStep).shortcut(".").enabled(active),
            act("Previous Frame", FrameBackStep).shortcut(",").enabled(active),
        ]);
        v
    }

    fn track_items(&self, kind: TrackKind, with_off: bool) -> Vec<M> {
        let st = self.player.state();
        let cur = match kind {
            TrackKind::Audio => st.selected.audio,
            TrackKind::Subtitle => st.selected.subtitle,
            TrackKind::Video => st.selected.video,
        };
        let mut v: Vec<M> = Vec::new();
        if with_off {
            v.push(M::action("Off", move |this, _, cx| this.select_track(kind, None, cx)).selected(cur.is_none()));
        }
        if let Some(m) = &st.media {
            for (n, t) in m.tracks_of(kind).filter(|t| !t.is_cover_art).enumerate() {
                let id = t.id;
                v.push(
                    M::action(t.label(n + 1), move |this, _, cx| this.select_track(kind, Some(id), cx))
                        .selected(cur == Some(id)),
                );
            }
        }
        v
    }

    fn menu_audio(&self) -> Vec<M> {
        let fx = self.settings.audio_fx.clone();
        let dev = self.settings.audio_device.clone();
        let mut devices = vec![
            M::action("System Default", |this, _, cx| {
                this.settings.audio_device = None;
                this.apply_settings(cx);
                this.reload_current(cx);
            })
            .selected(dev.is_none()),
        ];
        for d in self.audio_devices.iter() {
            let name = d.clone();
            let sel = dev.as_deref() == Some(d.as_str());
            devices.push(
                M::action(d.clone(), move |this, _, cx| {
                    this.settings.audio_device = Some(name.clone());
                    this.apply_settings(cx);
                    this.reload_current(cx);
                })
                .selected(sel),
            );
        }
        let ch = self.settings.channels;
        let chan = |label: &str, m: ChannelMode| {
            M::action(label.to_string(), move |this, _, cx| {
                this.settings.channels = m;
                this.apply_settings(cx);
                this.reload_current(cx);
            })
            .selected(ch == m)
        };
        vec![
            M::submenu("Audio Track", self.track_items(TrackKind::Audio, true)).icon(Icon::Audio),
            M::submenu("Audio Device", devices).icon(Icon::Speakers),
            M::submenu(
                "Channels",
                vec![
                    chan("Auto (match media)", ChannelMode::Auto),
                    chan("Stereo", ChannelMode::Stereo),
                    chan("Mono", ChannelMode::Mono),
                    chan("5.1 Surround", ChannelMode::Surround51),
                    chan("7.1 Surround", ChannelMode::Surround71),
                ],
            ),
            M::Separator,
            act("Volume Up", VolumeUp).shortcut("up"),
            act("Volume Down", VolumeDown).shortcut("down"),
            act("Mute", ToggleMute).shortcut("m").checked(self.player.muted()),
            M::Separator,
            act("Increase Audio Delay", AudioDelayIncrease).shortcut("secondary-="),
            act("Decrease Audio Delay", AudioDelayDecrease).shortcut("secondary--"),
            M::action("Reset Audio Delay", |this, _, cx| {
                this.player.set_audio_delay(0.0);
                this.osd("Audio delay 0 ms", cx)
            }),
            M::Separator,
            M::action("Normalize Volume", move |this, _, cx| {
                this.settings.audio_fx.normalize = !this.settings.audio_fx.normalize;
                let fx = this.settings.audio_fx.clone();
                this.player.set_audio_fx(fx);
                this.apply_settings(cx);
            })
            .checked(fx.normalize),
            M::action("Night Mode (compress dynamics)", move |this, _, cx| {
                this.settings.audio_fx.night_mode = !this.settings.audio_fx.night_mode;
                let fx = this.settings.audio_fx.clone();
                this.player.set_audio_fx(fx);
                this.apply_settings(cx);
            })
            .checked(fx.night_mode),
            act("Equalizer & Effects…", ShowEffects).shortcut("secondary-e"),
        ]
    }

    fn menu_video(&self) -> Vec<M> {
        let adj = self.video_adjust.clone();
        let aspect = self.aspect;
        let aspects: Vec<M> = ASPECTS
            .iter()
            .map(|&a| {
                M::action(a.label(), move |this, _, cx| {
                    this.aspect = a;
                    cx.notify()
                })
                .selected(aspect == a)
            })
            .collect();
        let zoom = self.zoom;
        let zooms: Vec<M> = [0.5f32, 0.75, 1.0, 1.25, 1.5, 2.0]
            .iter()
            .map(|&z| {
                M::action(format!("{:.0}%", z * 100.0), move |this, _, cx| {
                    this.zoom = z;
                    cx.notify()
                })
                .selected((zoom - z).abs() < 0.01)
            })
            .collect();
        let rot = |label: &str, r: Rotation| {
            M::action(label.to_string(), move |this, _, cx| {
                let mut a = this.video_adjust.clone();
                a.rotation = r;
                this.set_video_adjust(a, cx)
            })
            .selected(adj.rotation == r)
        };
        // Crop edges [left, top, right, bottom] (fractions) for a target aspect.
        fn crop_edges(target: f64, src: f64) -> [f32; 4] {
            if target > src {
                let c = ((1.0 - src / target) / 2.0) as f32;
                [0.0, c, 0.0, c]
            } else {
                let c = ((1.0 - target / src) / 2.0) as f32;
                [c, 0.0, c, 0.0]
            }
        }
        let src_aspect = self.player.with_state(|s| s.video.as_ref().map(|v| v.display_aspect)).unwrap_or(16.0 / 9.0);
        let crop = |label: &str, target: Option<f64>| {
            let sel = match (target, adj.crop) {
                (None, None) => true,
                (Some(t), Some(cur)) => crop_edges(t, src_aspect).iter().zip(cur).all(|(a, b)| (a - b).abs() < 1e-3),
                _ => false,
            };
            M::action(label.to_string(), move |this, _, cx| {
                let mut a = this.video_adjust.clone();
                let src = this.player.with_state(|s| s.video.as_ref().map(|v| v.display_aspect)).unwrap_or(16.0 / 9.0);
                a.crop = target.map(|t| crop_edges(t, src));
                this.set_video_adjust(a, cx)
            })
            .selected(sel)
        };
        let di = self.settings.deinterlace;
        let deint = |label: &str, d: Deinterlace| {
            M::action(label.to_string(), move |this, _, cx| {
                this.settings.deinterlace = d;
                this.apply_settings(cx)
            })
            .selected(di == d)
        };
        let hw = self.settings.hwdec;
        let hwm = |label: &str, h: HwDecode| {
            M::action(label.to_string(), move |this, _, cx| {
                this.settings.hwdec = h;
                this.apply_settings(cx);
                this.reload_current(cx)
            })
            .selected(hw == h)
        };
        vec![
            M::submenu("Video Track", self.track_items(TrackKind::Video, true)).icon(Icon::Video),
            act("Fullscreen", ToggleFullscreen).shortcut("f").icon(Icon::Fullscreen),
            act("Fit Window to Video", FitWindow).shortcut("secondary-0"),
            M::Separator,
            M::submenu("Aspect Ratio", aspects),
            M::submenu(
                "Crop",
                vec![
                    crop("None", None),
                    crop("16:9", Some(16.0 / 9.0)),
                    crop("4:3", Some(4.0 / 3.0)),
                    crop("2.35:1", Some(2.35)),
                    crop("2.39:1", Some(2.39)),
                    crop("1.85:1", Some(1.85)),
                    crop("1:1", Some(1.0)),
                ],
            ),
            M::submenu("Zoom", {
                let mut z = zooms;
                z.push(M::Separator);
                z.push(act("Zoom In", ZoomIn).shortcut("alt-="));
                z.push(act("Zoom Out", ZoomOut).shortcut("alt--"));
                z.push(act("Reset", ZoomReset).shortcut("alt-backspace"));
                z
            })
            .icon(Icon::ZoomIn),
            M::submenu(
                "Rotate",
                vec![
                    rot("0°", Rotation::None),
                    rot("90°", Rotation::R90),
                    rot("180°", Rotation::R180),
                    rot("270°", Rotation::R270),
                ],
            ),
            M::action("Flip Horizontally", |this, _, cx| {
                let mut a = this.video_adjust.clone();
                a.hflip = !a.hflip;
                this.set_video_adjust(a, cx)
            })
            .checked(adj.hflip),
            M::action("Flip Vertically", |this, _, cx| {
                let mut a = this.video_adjust.clone();
                a.vflip = !a.vflip;
                this.set_video_adjust(a, cx)
            })
            .checked(adj.vflip),
            M::Separator,
            M::submenu(
                "Deinterlace",
                vec![
                    deint("Automatic", Deinterlace::Auto),
                    deint("Always", Deinterlace::On),
                    deint("Off", Deinterlace::Off),
                ],
            ),
            M::submenu(
                "Hardware Decoding",
                vec![
                    hwm("Automatic (high resolutions)", HwDecode::Auto),
                    hwm("Always", HwDecode::Always),
                    hwm("Off", HwDecode::Off),
                ],
            ),
            M::action("HDR Tone Mapping", |this, _, cx| {
                this.settings.tone_map_hdr = !this.settings.tone_map_hdr;
                this.apply_settings(cx)
            })
            .checked(self.settings.tone_map_hdr),
            M::Separator,
            act("Take Screenshot", Screenshot).shortcut("s").icon(Icon::Screenshot).enabled(self.player.has_frame()),
            M::action("Picture Adjustments…", |this, w, cx| this.show_dialog(Dialog::Effects(2), w, cx)),
        ]
    }

    fn menu_subtitle(&self) -> Vec<M> {
        let style = self.settings.sub_style;
        vec![
            M::submenu("Subtitle Track", self.track_items(TrackKind::Subtitle, true)).icon(Icon::Subtitles),
            act("Load Subtitle File…", OpenSubtitle).shortcut("secondary-shift-o").enabled(self.player.is_active()),
            act("Show Subtitles", ToggleSubtitles).shortcut("v").checked(self.subs_visible),
            M::Separator,
            act("Delay +100 ms", SubDelayIncrease).shortcut("x"),
            act("Delay −100 ms", SubDelayDecrease).shortcut("z"),
            M::action("Reset Delay", |this, _, cx| {
                this.sub_delay = 0.0;
                this.osd("Subtitle delay 0 ms", cx)
            }),
            M::Separator,
            act("Larger", SubScaleUp).shortcut("shift-g"),
            act("Smaller", SubScaleDown).shortcut("shift-f"),
            M::submenu(
                "Style",
                vec![
                    M::action("Outlined text", |this, _, _| this.settings.sub_style = SubStyle::Outline)
                        .selected(style == SubStyle::Outline),
                    M::action("Text on dark box", |this, _, _| this.settings.sub_style = SubStyle::Box)
                        .selected(style == SubStyle::Box),
                ],
            ),
        ]
    }

    fn menu_view(&self) -> Vec<M> {
        let t = self.settings.theme;
        let theme = |label: &str, m: ThemeMode| {
            M::action(label.to_string(), move |this, w, cx| {
                this.settings.theme = m;
                this.theme = super::theme::Theme::resolve(m, w);
                this.apply_settings(cx)
            })
            .selected(t == m)
        };
        vec![
            act("Playlist", TogglePlaylist).shortcut("secondary-l").checked(self.show_playlist).icon(Icon::Playlist),
            act("Menu Bar", ToggleMenubar).shortcut("secondary-m").checked(self.settings.show_menubar),
            act("Fullscreen", ToggleFullscreen).shortcut("f"),
            M::Separator,
            M::submenu(
                "Theme",
                vec![
                    theme("Follow System", ThemeMode::System),
                    theme("Dark", ThemeMode::Dark),
                    theme("Light", ThemeMode::Light),
                ],
            ),
            M::Separator,
            act("Media Information", ShowMediaInfo).shortcut("i").icon(Icon::Info),
            act("Equalizer & Effects…", ShowEffects).shortcut("secondary-e"),
            act("Preferences…", ShowPreferences).shortcut("secondary-,").icon(Icon::Settings),
        ]
    }

    fn menu_help(&self) -> Vec<M> {
        vec![
            act("Keyboard Shortcuts", ShowShortcuts).shortcut("f1"),
            act("About MavPlayer", ShowAbout).icon(Icon::About),
        ]
    }

    /// Popup for the subtitle button in the control bar.
    pub fn subtitle_popup(&self) -> Vec<M> {
        let mut v = vec![M::Header("Subtitles".into())];
        v.extend(self.track_items(TrackKind::Subtitle, true));
        v.push(M::Separator);
        v.push(act("Load Subtitle File…", OpenSubtitle));
        v
    }

    pub fn audio_popup(&self) -> Vec<M> {
        let mut v = vec![M::Header("Audio track".into())];
        v.extend(self.track_items(TrackKind::Audio, false));
        v
    }

    pub fn speed_popup(&self) -> Vec<M> {
        let speed = self.player.speed();
        let mut v = vec![M::Header("Playback speed".into())];
        v.extend(SPEEDS.iter().map(|&s| {
            M::action(format!("{s}×"), move |this, _, cx| this.set_speed(s, cx)).selected((speed - s).abs() < 1e-3)
        }));
        v
    }
}
