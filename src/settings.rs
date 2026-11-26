use std::num::NonZeroU32;
use std::sync::Arc;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use softbuffer::{Context, Surface};
use tiny_skia::{FillRule, FilterQuality, PathBuilder, Pixmap, PixmapPaint, Transform};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Theme, Window, WindowId};

use crate::config::{
    AppConfig, SaveFormat, format_hotkey_label, winit_keycode_to_hotkey_code,
};
use crate::tools::PixelRect;
use crate::ui::{
    blit_pixmap, draw_bitmap_text, draw_rounded_rect, fill_solid_rect, paint_rgba,
    stroke_rounded_rect, text_pixel_width,
};

const LOGICAL_W: u32 = 504;
const LOGICAL_H: u32 = 500;

pub enum SettingsAction {
    None,
    ApplyHotkey(HotKey),
    SelectFormat(SaveFormat),
    ToggleAutostart(bool),
    CheckUpdates,
    TakeScreenshot,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsHit {
    RecordHotkey,
    ResetHotkey,
    Format(SaveFormat),
    ToggleAutostart,
    CheckUpdates,
    TakeScreenshot,
    Close,
}

struct HitItem {
    rect: PixelRect,
    hit: SettingsHit,
}

/// Lightweight native Settings window rendered via `softbuffer` + `tiny-skia`.
pub struct SettingsWindow {
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    recording: bool,
    modifiers: ModifiersState,
    autostart: bool,
    mouse_logical: (i32, i32),
    hover: Option<SettingsHit>,
    current_cursor: CursorIcon,
    buttons: Vec<HitItem>,
    logical_pix: Pixmap,
    scaled_pix: Option<Pixmap>,
    ui_dirty: bool,
    last_phys: (u32, u32),
    status_msg: String,
    status_ok: bool,
}

impl SettingsWindow {
    pub fn create(
        event_loop: &ActiveEventLoop,
        config: &AppConfig,
        hotkey_ok: bool,
        hotkey_err: Option<&str>,
        autostart: bool,
    ) -> Result<Self, String> {
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("FineShot Settings")
            .with_inner_size(LogicalSize::new(LOGICAL_W as f64, LOGICAL_H as f64))
            .with_theme(Some(Theme::Dark))
            .with_resizable(false)
            .with_visible(false);

        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_drag_and_drop(false);
        }

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| format!("Failed to create settings window: {e}"))?,
        );

        let context = Context::new(window.clone())
            .map_err(|e| format!("Failed to create softbuffer context: {e}"))?;
        let mut surface = Surface::new(&context, window.clone())
            .map_err(|e| format!("Failed to create softbuffer surface: {e}"))?;

        let size = window.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            let _ = surface.resize(w, h);
        }

        let (status_msg, status_ok) = if hotkey_ok {
            ("Global shortcut active".to_string(), true)
        } else {
            (
                hotkey_err
                    .unwrap_or("Shortcut unavailable - click Change to pick another")
                    .to_string(),
                false,
            )
        };

        let mut win = Self {
            window,
            surface,
            recording: false,
            modifiers: ModifiersState::empty(),
            autostart,
            mouse_logical: (0, 0),
            hover: None,
            current_cursor: CursorIcon::Default,
            buttons: Vec::new(),
            logical_pix: Pixmap::new(LOGICAL_W, LOGICAL_H).unwrap(),
            scaled_pix: None,
            ui_dirty: true,
            last_phys: (0, 0),
            status_msg,
            status_ok,
        };
        win.render(config);
        win.window.set_visible(true);
        win.window.focus_window();
        Ok(win)
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn focus(&self) {
        self.window.focus_window();
    }

    pub fn set_autostart(&mut self, enabled: bool) {
        self.autostart = enabled;
        self.ui_dirty = true;
        self.window.request_redraw();
    }

    pub fn set_status(&mut self, ok: bool, msg: impl Into<String>) {
        self.status_ok = ok;
        self.status_msg = msg.into();
        self.recording = false;
        self.ui_dirty = true;
        self.window.request_redraw();
    }

    pub fn mark_dirty_and_redraw(&mut self) {
        self.ui_dirty = true;
        self.window.request_redraw();
    }

    pub fn handle_event(&mut self, event: WindowEvent, config: &AppConfig) -> SettingsAction {
        match event {
            WindowEvent::CloseRequested => SettingsAction::Close,
            WindowEvent::Resized(size) => {
                if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    let _ = self.surface.resize(w, h);
                    self.ui_dirty = true;
                    self.window.request_redraw();
                }
                SettingsAction::None
            }
            WindowEvent::RedrawRequested => {
                self.render(config);
                SettingsAction::None
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                if self.recording {
                    self.ui_dirty = true;
                    self.window.request_redraw();
                }
                SettingsAction::None
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.window.scale_factor();
                let lx = (position.x / scale).round() as i32;
                let ly = (position.y / scale).round() as i32;
                self.mouse_logical = (lx, ly);
                let prev = self.hover;
                self.hover = self
                    .buttons
                    .iter()
                    .find(|b| b.rect.contains(lx, ly))
                    .map(|b| b.hit);
                let target_cursor = if self.hover.is_some() {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Default
                };
                if self.current_cursor != target_cursor {
                    self.current_cursor = target_cursor;
                    self.window.set_cursor(target_cursor);
                }
                if prev != self.hover {
                    self.ui_dirty = true;
                    self.window.request_redraw();
                }
                SettingsAction::None
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (lx, ly) = self.mouse_logical;
                let hit = self
                    .buttons
                    .iter()
                    .find(|b| b.rect.contains(lx, ly))
                    .map(|b| b.hit);
                match hit {
                    Some(SettingsHit::RecordHotkey) => {
                        self.recording = !self.recording;
                        self.ui_dirty = true;
                        self.window.request_redraw();
                        SettingsAction::None
                    }
                    Some(SettingsHit::ResetHotkey) => {
                        self.recording = false;
                        self.ui_dirty = true;
                        SettingsAction::ApplyHotkey(HotKey::new(None, Code::PrintScreen))
                    }
                    Some(SettingsHit::Format(fmt)) => {
                        self.recording = false;
                        self.ui_dirty = true;
                        SettingsAction::SelectFormat(fmt)
                    }
                    Some(SettingsHit::ToggleAutostart) => {
                        self.recording = false;
                        self.ui_dirty = true;
                        SettingsAction::ToggleAutostart(!self.autostart)
                    }
                    Some(SettingsHit::CheckUpdates) => {
                        self.recording = false;
                        SettingsAction::CheckUpdates
                    }
                    Some(SettingsHit::TakeScreenshot) => {
                        self.recording = false;
                        SettingsAction::TakeScreenshot
                    }
                    Some(SettingsHit::Close) => {
                        self.recording = false;
                        SettingsAction::Close
                    }
                    None => {
                        if self.recording {
                            self.recording = false;
                            self.ui_dirty = true;
                            self.window.request_redraw();
                        }
                        SettingsAction::None
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if !self.recording {
                    if matches!(event.logical_key, Key::Named(NamedKey::Escape)) {
                        return SettingsAction::Close;
                    }
                    return SettingsAction::None;
                }
                if matches!(event.logical_key, Key::Named(NamedKey::Escape)) {
                    self.recording = false;
                    self.ui_dirty = true;
                    self.window.request_redraw();
                    return SettingsAction::None;
                }
                if let PhysicalKey::Code(key_code) = event.physical_key {
                    if let Some(hk_code) = winit_keycode_to_hotkey_code(key_code) {
                        let mut mods = Modifiers::empty();
                        if self.modifiers.control_key() {
                            mods |= Modifiers::CONTROL;
                        }
                        if self.modifiers.shift_key() {
                            mods |= Modifiers::SHIFT;
                        }
                        if self.modifiers.alt_key() {
                            mods |= Modifiers::ALT;
                        }
                        if self.modifiers.super_key() {
                            mods |= Modifiers::META;
                        }
                        let hk = HotKey::new(
                            if mods.is_empty() { None } else { Some(mods) },
                            hk_code,
                        );
                        self.recording = false;
                        return SettingsAction::ApplyHotkey(hk);
                    }
                }
                SettingsAction::None
            }
            _ => SettingsAction::None,
        }
    }

    fn render(&mut self, config: &AppConfig) {
        let phys = self.window.inner_size();
        if phys.width == 0 || phys.height == 0 {
            return;
        }

        if self.ui_dirty || self.last_phys != (phys.width, phys.height) {
            if self.logical_pix.width() != LOGICAL_W || self.logical_pix.height() != LOGICAL_H {
                self.logical_pix = Pixmap::new(LOGICAL_W, LOGICAL_H).unwrap();
            }
            paint_settings_ui(
                &mut self.logical_pix,
                &mut self.buttons,
                config,
                self.recording,
                &self.modifiers,
                self.autostart,
                self.hover,
                &self.status_msg,
                self.status_ok,
            );

            // Scale to physical DPI only when UI is dirty or size changed, reusing scaled_pix
            if phys.width != LOGICAL_W || phys.height != LOGICAL_H {
                let needs_new = !matches!(&self.scaled_pix, Some(s) if s.width() == phys.width && s.height() == phys.height);
                if needs_new {
                    self.scaled_pix = Some(Pixmap::new(phys.width, phys.height).unwrap());
                }
                let scaled = self.scaled_pix.as_mut().unwrap();
                let sx = phys.width as f32 / LOGICAL_W as f32;
                let sy = phys.height as f32 / LOGICAL_H as f32;
                let paint = PixmapPaint {
                    quality: FilterQuality::Bilinear,
                    ..PixmapPaint::default()
                };
                scaled.draw_pixmap(
                    0,
                    0,
                    self.logical_pix.as_ref(),
                    &paint,
                    Transform::from_scale(sx, sy),
                    None,
                );
            } else {
                self.scaled_pix = None;
            }
            self.ui_dirty = false;
            self.last_phys = (phys.width, phys.height);
        }

        let src_pix = if phys.width == LOGICAL_W && phys.height == LOGICAL_H {
            &self.logical_pix
        } else {
            self.scaled_pix.as_ref().unwrap_or(&self.logical_pix)
        };

        if let Ok(mut buffer) = self.surface.buffer_mut() {
            let pw = phys.width as i32;
            let ph = phys.height as i32;
            blit_pixmap(&mut buffer, pw, ph, src_pix, 0, 0);
            let _ = buffer.present();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_settings_ui(
    pix: &mut Pixmap,
    buttons: &mut Vec<HitItem>,
    config: &AppConfig,
    recording: bool,
    modifiers: &ModifiersState,
    autostart: bool,
    hover: Option<SettingsHit>,
    status_msg: &str,
    status_ok: bool,
) {
    buttons.clear();

    const MARGIN: i32 = 24;
    const CARD_W: i32 = LOGICAL_W as i32 - MARGIN * 2; // 456
    const PAD: i32 = 16;
    const INNER_X: i32 = MARGIN + PAD; // 40
    const INNER_W: i32 = CARD_W - PAD * 2; // 424
    const INNER_R: i32 = INNER_X + INNER_W; // 464
    const BTN_H: i32 = 36;

    // Window background
    fill_solid_rect(pix, 0.0, 0.0, LOGICAL_W as f32, LOGICAL_H as f32, [18, 20, 25, 255]);

    // Header
    draw_bitmap_text(pix, "FineShot Settings", MARGIN, 20, 2, [248, 250, 252, 255]);
    let ver = format!("v{}", crate::updater::CURRENT_VERSION);
    let ver_w = text_pixel_width(&ver, 1);
    draw_bitmap_text(
        pix,
        &ver,
        MARGIN + CARD_W - ver_w,
        24,
        1,
        [100, 116, 139, 255],
    );
    draw_bitmap_text(
        pix,
        "Configure shortcut, format & startup",
        MARGIN,
        42,
        1,
        [148, 163, 184, 255],
    );

    // Card 1: Screenshot Hotkey (y = 66..170, h = 104)
    draw_card(pix, MARGIN, 66, CARD_W, 104);
    draw_bitmap_text(pix, "SCREENSHOT HOTKEY", INNER_X, 80, 1, [148, 163, 184, 255]);

    let box_rect = PixelRect {
        x: INNER_X,
        y: 98,
        w: 232,
        h: BTN_H,
    };
    let box_border = if recording {
        [56, 189, 248, 255]
    } else {
        [63, 68, 81, 255]
    };
    draw_rounded_rect(
        pix,
        box_rect.x as f32,
        box_rect.y as f32,
        box_rect.w as f32,
        box_rect.h as f32,
        7.0,
        [20, 22, 27, 255],
    );
    stroke_rounded_rect(
        pix,
        box_rect.x as f32,
        box_rect.y as f32,
        box_rect.w as f32,
        box_rect.h as f32,
        7.0,
        box_border,
        1.5,
    );

    let hotkey_text = if recording {
        let mut prefix = Vec::new();
        if modifiers.control_key() {
            prefix.push("Ctrl");
        }
        if modifiers.shift_key() {
            prefix.push("Shift");
        }
        if modifiers.alt_key() {
            prefix.push("Alt");
        }
        if prefix.is_empty() {
            "Press any key...".to_string()
        } else {
            format!("{} + ...", prefix.join(" + "))
        }
    } else {
        format_hotkey_label(config.parsed_hotkey())
    };
    let text_col = if recording {
        [56, 189, 248, 255]
    } else {
        [248, 250, 252, 255]
    };
    draw_bitmap_text(pix, &hotkey_text, INNER_X + 12, 109, 2, text_col);

    // Change / Cancel button (x = 280, w = 96)
    let rec_rect = PixelRect {
        x: INNER_X + 240,
        y: 98,
        w: 96,
        h: BTN_H,
    };
    let rec_bg = if recording {
        [14, 165, 233, 255]
    } else if hover == Some(SettingsHit::RecordHotkey) {
        [59, 130, 246, 255]
    } else {
        [37, 99, 235, 255]
    };
    let rec_label = if recording { "Cancel" } else { "Change" };
    draw_button(pix, rec_rect, rec_bg, None, rec_label, [255, 255, 255, 255]);
    buttons.push(HitItem {
        rect: rec_rect,
        hit: SettingsHit::RecordHotkey,
    });

    // Reset button (x = 384, w = 80 -> right edge 464, 11px padding per side)
    let rst_rect = PixelRect {
        x: INNER_X + 344,
        y: 98,
        w: 80,
        h: BTN_H,
    };
    let rst_bg = if hover == Some(SettingsHit::ResetHotkey) {
        [63, 68, 81, 255]
    } else {
        [43, 47, 56, 255]
    };
    draw_button(
        pix,
        rst_rect,
        rst_bg,
        Some([63, 68, 81, 255]),
        "Reset",
        [226, 232, 240, 255],
    );
    buttons.push(HitItem {
        rect: rst_rect,
        hit: SettingsHit::ResetHotkey,
    });

    // Status dot + text
    let status_col = if status_ok {
        [74, 222, 128, 255]
    } else {
        [251, 191, 36, 255]
    };
    if let Some(dot) = PathBuilder::from_circle((INNER_X + 4) as f32, 150.5, 3.5) {
        pix.fill_path(
            &dot,
            &paint_rgba(status_col),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    draw_bitmap_text(pix, status_msg, INNER_X + 14, 147, 1, status_col);

    // Card 2: Default Save Format (y = 184..316, h = 132)
    draw_card(pix, MARGIN, 184, CARD_W, 132);
    draw_bitmap_text(pix, "DEFAULT SAVE FORMAT", INNER_X, 198, 1, [148, 163, 184, 255]);
    let fmt_hint = "All 8 stay in Save As";
    let fmt_hint_w = text_pixel_width(fmt_hint, 1);
    draw_bitmap_text(
        pix,
        fmt_hint,
        INNER_R - fmt_hint_w,
        198,
        1,
        [100, 116, 139, 255],
    );

    let cols = 4;
    let chip_w = 100; // 4 * 100 + 3 * 8 = 424
    let gap = 8;
    let fmt_y0 = 218;

    for (i, &fmt) in SaveFormat::ALL.iter().enumerate() {
        let col_idx = (i % cols) as i32;
        let row_idx = (i / cols) as i32;
        let rc = PixelRect {
            x: INNER_X + col_idx * (chip_w + gap),
            y: fmt_y0 + row_idx * (BTN_H + gap),
            w: chip_w,
            h: BTN_H,
        };
        let selected = config.default_format == fmt;
        let hovered = hover == Some(SettingsHit::Format(fmt));
        draw_option_pill(pix, rc, selected, hovered, fmt.short_label());
        buttons.push(HitItem {
            rect: rc,
            hit: SettingsHit::Format(fmt),
        });
    }

    // Card 3: Windows Startup (y = 330..418, h = 88)
    draw_card(pix, MARGIN, 330, CARD_W, 88);
    draw_bitmap_text(pix, "WINDOWS STARTUP", INNER_X, 344, 1, [148, 163, 184, 255]);
    let startup_hint = "Run in tray at sign-in";
    let startup_hint_w = text_pixel_width(startup_hint, 1);
    draw_bitmap_text(
        pix,
        startup_hint,
        INNER_R - startup_hint_w,
        344,
        1,
        [100, 116, 139, 255],
    );

    draw_bitmap_text(
        pix,
        "Start with Windows",
        INNER_X,
        364 + (BTN_H - 14) / 2,
        2,
        [226, 232, 240, 255],
    );

    let startup_rect = PixelRect {
        x: INNER_R - 120,
        y: 364,
        w: 120,
        h: BTN_H,
    };
    let startup_hovered = hover == Some(SettingsHit::ToggleAutostart);
    let startup_label = if autostart { "Enabled" } else { "Disabled" };
    draw_option_pill(pix, startup_rect, autostart, startup_hovered, startup_label);
    buttons.push(HitItem {
        rect: startup_rect,
        hit: SettingsHit::ToggleAutostart,
    });

    // Footer (y = 438..476) flush with card margins
    let shot_rect = PixelRect {
        x: MARGIN,
        y: 438,
        w: 200,
        h: 38,
    };
    let shot_bg = if hover == Some(SettingsHit::TakeScreenshot) {
        [22, 163, 74, 255]
    } else {
        [34, 197, 94, 245]
    };
    draw_button(
        pix,
        shot_rect,
        shot_bg,
        None,
        "Take Screenshot",
        [255, 255, 255, 255],
    );
    buttons.push(HitItem {
        rect: shot_rect,
        hit: SettingsHit::TakeScreenshot,
    });

    let upd_rect = PixelRect {
        x: MARGIN + 208,
        y: 438,
        w: 120,
        h: 38,
    };
    let upd_bg = if hover == Some(SettingsHit::CheckUpdates) {
        [63, 68, 81, 255]
    } else {
        [43, 47, 56, 255]
    };
    draw_button(
        pix,
        upd_rect,
        upd_bg,
        Some([63, 68, 81, 255]),
        "Updates",
        [226, 232, 240, 255],
    );
    buttons.push(HitItem {
        rect: upd_rect,
        hit: SettingsHit::CheckUpdates,
    });

    let done_rect = PixelRect {
        x: MARGIN + CARD_W - 120,
        y: 438,
        w: 120,
        h: 38,
    };
    let done_bg = if hover == Some(SettingsHit::Close) {
        [63, 68, 81, 255]
    } else {
        [43, 47, 56, 255]
    };
    draw_button(
        pix,
        done_rect,
        done_bg,
        Some([63, 68, 81, 255]),
        "Done",
        [241, 245, 249, 255],
    );
    buttons.push(HitItem {
        rect: done_rect,
        hit: SettingsHit::Close,
    });
}

fn draw_card(pix: &mut Pixmap, x: i32, y: i32, w: i32, h: i32) {
    draw_rounded_rect(pix, x as f32, y as f32, w as f32, h as f32, 10.0, [28, 31, 38, 255]);
    stroke_rounded_rect(
        pix,
        x as f32 + 0.5,
        y as f32 + 0.5,
        w as f32 - 1.0,
        h as f32 - 1.0,
        10.0,
        [46, 51, 62, 255],
        1.0,
    );
}

fn draw_option_pill(pix: &mut Pixmap, rc: PixelRect, selected: bool, hovered: bool, label: &str) {
    let (bg, border) = if selected && hovered {
        ([59, 130, 246, 255], Some([147, 197, 253, 255]))
    } else if selected {
        ([37, 99, 235, 255], Some([96, 165, 250, 255]))
    } else if hovered {
        ([55, 62, 74, 255], Some([75, 85, 99, 255]))
    } else {
        ([38, 42, 51, 255], Some([54, 59, 71, 255]))
    };
    let fg = if selected {
        [255, 255, 255, 255]
    } else {
        [226, 232, 240, 255]
    };
    draw_button(pix, rc, bg, border, label, fg);
}

fn draw_button(
    pix: &mut Pixmap,
    rc: PixelRect,
    bg: [u8; 4],
    border: Option<[u8; 4]>,
    label: &str,
    fg: [u8; 4],
) {
    draw_rounded_rect(pix, rc.x as f32, rc.y as f32, rc.w as f32, rc.h as f32, 7.0, bg);
    if let Some(b) = border {
        stroke_rounded_rect(
            pix,
            rc.x as f32 + 0.5,
            rc.y as f32 + 0.5,
            rc.w as f32 - 1.0,
            rc.h as f32 - 1.0,
            7.0,
            b,
            1.0,
        );
    }
    let tw = text_pixel_width(label, 2);
    draw_bitmap_text(
        pix,
        label,
        rc.x + (rc.w - tw) / 2,
        rc.y + (rc.h - 14) / 2,
        2,
        fg,
    );
}
