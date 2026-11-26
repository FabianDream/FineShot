use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::RgbaImage;
use softbuffer::{Context, Surface};
use tiny_skia::{FillRule, LineCap, LineJoin, PathBuilder, Pixmap, Rect, Stroke, Transform};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};

use crate::capture::{CapturedMonitor, VirtualCapture};
use crate::tools::{
    Annotation, AnnotationStack, BlurScratch, PALETTE, PixelRect, STROKE_WIDTHS, ToolKind,
    apply_annotations_onto_with_scratch, composite_region, draw_pen_segment,
};
use crate::ui::{
    blit_pixmap_clipped, draw_bitmap_text, draw_rounded_rect, fill_solid_rect, paint_rgba,
    stroke_rounded_rect,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Outcome returned by overlay input handling so the application loop can export or close.
pub enum OverlayAction {
    None,
    CopyAndClose,
    SaveAsync(RgbaImage),
    OpenSettings,
    CancelAndClose,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ResizeHandle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DragMode {
    None,
    NewSelection { anchor_x: i32, anchor_y: i32 },
    MoveSelection { start_x: i32, start_y: i32, orig: PixelRect },
    ResizeSelection { handle: ResizeHandle, orig: PixelRect },
    DrawAnnotation,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToolbarHit {
    Tool(ToolKind),
    Color(usize),
    Thickness,
    Undo,
    Settings,
    Save,
    Copy,
    Cancel,
}

struct ToolbarButton {
    rect: PixelRect,
    hit: ToolbarHit,
}

struct CachedToolbar {
    pixmap: Pixmap,
    buttons: Vec<ToolbarButton>,
    tool: ToolKind,
    color_idx: usize,
    thickness_idx: usize,
    can_undo: bool,
    hover: Option<ToolbarHit>,
}

struct MonitorWindow {
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    bounds: PixelRect,
    surface_w: u32,
    surface_h: u32,
    has_painted_base: bool,
    needs_full_redraw: bool,
    last_rendered_size: (u32, u32),
    current_cursor: CursorIcon,
}

/// Interactive per-monitor borderless overlay powered by `winit`, `softbuffer`, and `tiny-skia`.
/// Multi-monitor setups spawn one window per monitor and only update/present monitors touched by damage rects.
pub struct OverlaySession {
    windows: Vec<MonitorWindow>,
    capture: VirtualCapture,
    selection: Option<PixelRect>,
    drag: DragMode,
    mouse_pos: (i32, i32),
    modifiers: ModifiersState,
    active_tool: ToolKind,
    color_idx: usize,
    thickness_idx: usize,
    stack: AnnotationStack,
    commit_gen: u64,
    cached_commit: Option<(u64, PixelRect, RgbaImage)>,
    live_scratch: Option<RgbaImage>,
    blur_scratch: BlurScratch,
    cached_toolbar: Option<CachedToolbar>,
    cached_badge: Option<(i32, i32, Pixmap)>,
    cached_hint: Option<Pixmap>,
    toolbar_rect: Option<PixelRect>,
    prev_selection: Option<PixelRect>,
    prev_badge_rect: Option<PixelRect>,
    prev_hint_rect: Option<PixelRect>,
    prev_active_dirty: Option<PixelRect>,
    pending_pen_dirty: Option<PixelRect>,
    damage_rects: Vec<PixelRect>,
    soft_rects: Vec<softbuffer::Rect>,
    needs_scene_damage: bool,
    pending_render_cycle: bool,
    last_render: Instant,
    redraw_pending: bool,
    hover_hit: Option<ToolbarHit>,
    exporting_dialog: bool,
}

impl OverlaySession {
    pub fn create(event_loop: &ActiveEventLoop, capture: VirtualCapture) -> Result<Self, String> {
        let mut windows = Vec::with_capacity(capture.monitors.len());

        for mon in &capture.monitors {
            #[allow(unused_mut)]
            let mut attrs = Window::default_attributes()
                .with_title("FineShot")
                .with_decorations(false)
                .with_resizable(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_position(PhysicalPosition::new(mon.screen_x, mon.screen_y))
                .with_inner_size(PhysicalSize::new(
                    mon.bounds.w as u32,
                    mon.bounds.h as u32,
                ))
                .with_visible(false);

            #[cfg(target_os = "windows")]
            {
                use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
                attrs = attrs
                    .with_skip_taskbar(true)
                    .with_drag_and_drop(false)
                    .with_undecorated_shadow(false)
                    .with_corner_preference(CornerPreference::DoNotRound);
            }

            let window = Arc::new(
                event_loop
                    .create_window(attrs)
                    .map_err(|e| format!("Failed to create overlay window: {e}"))?,
            );

            disable_window_animations(&window);

            window.set_outer_position(PhysicalPosition::new(mon.screen_x, mon.screen_y));
            let _ = window.request_inner_size(PhysicalSize::new(
                mon.bounds.w as u32,
                mon.bounds.h as u32,
            ));
            window.set_cursor(CursorIcon::Crosshair);

            let context = Context::new(window.clone())
                .map_err(|e| format!("Failed to create softbuffer context: {e}"))?;
            let mut surface = Surface::new(&context, window.clone())
                .map_err(|e| format!("Failed to create softbuffer surface: {e}"))?;

            let surf_w = NonZeroU32::new(mon.bounds.w.max(1) as u32).unwrap();
            let surf_h = NonZeroU32::new(mon.bounds.h.max(1) as u32).unwrap();
            surface
                .resize(surf_w, surf_h)
                .map_err(|e| format!("Failed to resize softbuffer surface: {e}"))?;

            windows.push(MonitorWindow {
                window,
                surface,
                bounds: mon.bounds,
                surface_w: surf_w.get(),
                surface_h: surf_h.get(),
                has_painted_base: false,
                needs_full_redraw: true,
                last_rendered_size: (0, 0),
                current_cursor: CursorIcon::Crosshair,
            });
        }

        let mut session = Self {
            windows,
            capture,
            selection: None,
            drag: DragMode::None,
            mouse_pos: (i32::MIN, i32::MIN),
            modifiers: ModifiersState::empty(),
            active_tool: ToolKind::Select,
            color_idx: 0,
            thickness_idx: 1,
            stack: AnnotationStack::default(),
            commit_gen: 0,
            cached_commit: None,
            live_scratch: None,
            blur_scratch: BlurScratch::default(),
            cached_toolbar: None,
            cached_badge: None,
            cached_hint: None,
            toolbar_rect: None,
            prev_selection: None,
            prev_badge_rect: None,
            prev_hint_rect: None,
            prev_active_dirty: None,
            pending_pen_dirty: None,
            damage_rects: Vec::with_capacity(8),
            soft_rects: Vec::with_capacity(8),
            needs_scene_damage: true,
            pending_render_cycle: false,
            last_render: Instant::now() - FRAME_INTERVAL,
            redraw_pending: false,
            hover_hit: None,
            exporting_dialog: false,
        };

        // Paint the initial frozen desktop onto all monitor surfaces before revealing them.
        session.render();
        for mw in &session.windows {
            mw.window.set_visible(true);
        }
        if let Some(first) = session.windows.first() {
            first.window.focus_window();
        }

        Ok(session)
    }

    pub fn owns_window(&self, window_id: WindowId) -> bool {
        self.windows.iter().any(|w| w.window.id() == window_id)
    }

    pub fn hide_immediately(&self) {
        for mw in &self.windows {
            mw.window.set_visible(false);
        }
    }

    /// Consumes the session (immediately dropping all monitor windows and softbuffer surfaces)
    /// and returns the captured buffer, export rectangle, and annotations for async clipboard copy.
    pub fn into_export_parts(self) -> (VirtualCapture, PixelRect, Vec<Annotation>) {
        let sel = self.effective_export_rect();
        (self.capture, sel, self.stack.items)
    }

    /// Returns the next frame deadline if a throttled redraw is pending.
    pub fn poll_redraw_deadline(&mut self) -> Option<Instant> {
        if !self.redraw_pending {
            return None;
        }
        let deadline = self.last_render + FRAME_INTERVAL;
        if Instant::now() >= deadline {
            self.redraw_pending = false;
            self.pending_render_cycle = true;
            if let Some(first) = self.windows.first() {
                first.window.request_redraw();
            }
            None
        } else {
            Some(deadline)
        }
    }

    pub fn set_save_dialog_active(&mut self, active: bool) {
        self.exporting_dialog = active;
        if active {
            self.cached_commit = None;
            self.live_scratch = None;
            for mw in &self.windows {
                mw.window.set_window_level(WindowLevel::Normal);
            }
        } else {
            for mw in &mut self.windows {
                mw.window.set_window_level(WindowLevel::AlwaysOnTop);
                mw.has_painted_base = false;
                mw.needs_full_redraw = true;
            }
            if let Some(first) = self.windows.first() {
                first.window.focus_window();
                first.window.request_redraw();
            }
            self.needs_scene_damage = true;
            self.pending_render_cycle = true;
            self.redraw_pending = false;
        }
    }

    fn schedule_redraw(&mut self, scene_damage: bool) {
        if scene_damage {
            self.needs_scene_damage = true;
        }
        let any_unpainted = self.windows.iter().any(|w| !w.has_painted_base);
        if any_unpainted || self.last_render.elapsed() >= FRAME_INTERVAL {
            self.redraw_pending = false;
            self.pending_render_cycle = true;
            if let Some(first) = self.windows.first() {
                first.window.request_redraw();
            }
        } else {
            self.redraw_pending = true;
        }
    }

    fn set_cursor_cached(&mut self, win_idx: usize, icon: CursorIcon) {
        if let Some(mw) = self.windows.get_mut(win_idx) {
            if mw.current_cursor != icon {
                mw.current_cursor = icon;
                mw.window.set_cursor(icon);
            }
        }
    }

    pub fn handle_window_event(&mut self, window_id: WindowId, event: WindowEvent) -> OverlayAction {
        let Some(win_idx) = self.windows.iter().position(|w| w.window.id() == window_id) else {
            return OverlayAction::None;
        };

        match event {
            WindowEvent::RedrawRequested => {
                if !self.pending_render_cycle {
                    self.windows[win_idx].needs_full_redraw = true;
                }
                self.pending_render_cycle = false;
                self.redraw_pending = false;
                self.render();
                OverlayAction::None
            }
            WindowEvent::Resized(size) => {
                if let (Some(w), Some(h)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    let mw = &mut self.windows[win_idx];
                    mw.surface_w = w.get();
                    mw.surface_h = h.get();
                    let _ = mw.surface.resize(w, h);
                    mw.has_painted_base = false;
                    mw.needs_full_redraw = true;
                    self.schedule_redraw(true);
                }
                OverlayAction::None
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                let mon = &self.capture.monitors[win_idx];
                let mw = &mut self.windows[win_idx];
                mw.window
                    .set_outer_position(PhysicalPosition::new(mon.screen_x, mon.screen_y));
                let _ = mw.window.request_inner_size(PhysicalSize::new(
                    mon.bounds.w as u32,
                    mon.bounds.h as u32,
                ));
                mw.has_painted_base = false;
                mw.needs_full_redraw = true;
                self.schedule_redraw(true);
                OverlayAction::None
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                OverlayAction::None
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if self.exporting_dialog {
                    return OverlayAction::None;
                }
                self.handle_key(event.logical_key, event.physical_key)
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.exporting_dialog {
                    return OverlayAction::None;
                }
                let mon_bounds = self.windows[win_idx].bounds;
                let mx = (mon_bounds.x + position.x.round() as i32)
                    .clamp(0, self.capture.width as i32 - 1);
                let my = (mon_bounds.y + position.y.round() as i32)
                    .clamp(0, self.capture.height as i32 - 1);
                if (mx, my) == self.mouse_pos {
                    return OverlayAction::None;
                }
                self.on_cursor_moved(win_idx, mx, my);
                OverlayAction::None
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.exporting_dialog {
                    return OverlayAction::None;
                }
                match button {
                    MouseButton::Right if state == ElementState::Pressed => {
                        OverlayAction::CancelAndClose
                    }
                    MouseButton::Left => self.on_left_mouse(win_idx, state),
                    _ => OverlayAction::None,
                }
            }
            WindowEvent::CloseRequested => OverlayAction::CancelAndClose,
            _ => OverlayAction::None,
        }
    }

    fn handle_key(&mut self, logical_key: Key, physical_key: PhysicalKey) -> OverlayAction {
        let ctrl = self.modifiers.control_key();

        if matches!(logical_key, Key::Named(NamedKey::Escape)) {
            return OverlayAction::CancelAndClose;
        }
        if matches!(logical_key, Key::Named(NamedKey::Enter)) {
            return OverlayAction::CopyAndClose;
        }

        if ctrl {
            if let PhysicalKey::Code(code) = physical_key {
                match code {
                    KeyCode::KeyC => return OverlayAction::CopyAndClose,
                    KeyCode::KeyS => return self.build_save_action(),
                    KeyCode::KeyZ => {
                        if self.stack.undo() {
                            self.commit_gen += 1;
                            self.prev_active_dirty = None;
                            self.pending_pen_dirty = None;
                            self.schedule_redraw(true);
                        }
                        return OverlayAction::None;
                    }
                    KeyCode::KeyA => {
                        let next_sel = Some(PixelRect {
                            x: 0,
                            y: 0,
                            w: self.capture.width as i32,
                            h: self.capture.height as i32,
                        });
                        if next_sel != self.selection {
                            self.selection = next_sel;
                            self.drag = DragMode::None;
                            self.schedule_redraw(true);
                        }
                        return OverlayAction::None;
                    }
                    _ => {}
                }
            }
        }

        OverlayAction::None
    }

    fn build_save_action(&self) -> OverlayAction {
        let sel = self.effective_export_rect();
        let img = composite_region(&self.capture, sel, &self.stack.items, None);
        OverlayAction::SaveAsync(img)
    }

    fn effective_export_rect(&self) -> PixelRect {
        self.selection
            .filter(|r| r.w > 0 && r.h > 0)
            .unwrap_or(PixelRect {
                x: 0,
                y: 0,
                w: self.capture.width as i32,
                h: self.capture.height as i32,
            })
    }

    fn on_left_mouse(&mut self, win_idx: usize, state: ElementState) -> OverlayAction {
        let (mx, my) = if self.mouse_pos.0 == i32::MIN {
            (0, 0)
        } else {
            self.mouse_pos
        };

        if state == ElementState::Pressed {
            if let Some(hit) = self.hit_test_toolbar(mx, my) {
                return self.activate_toolbar_hit(hit);
            }

            if let Some(sel) = self.selection {
                if let Some(handle) = hit_test_handles(sel, mx, my) {
                    self.drag = DragMode::ResizeSelection { handle, orig: sel };
                    self.schedule_redraw(true);
                    return OverlayAction::None;
                }

                if sel.contains(mx, my) {
                    if self.active_tool != ToolKind::Select {
                        self.start_annotation(mx as f32, my as f32);
                        self.drag = DragMode::DrawAnnotation;
                        self.pending_pen_dirty = None;
                        self.prev_active_dirty = self.stack.active.as_ref().map(annotation_bounds);
                        self.schedule_redraw(true);
                        return OverlayAction::None;
                    } else {
                        self.drag = DragMode::MoveSelection {
                            start_x: mx,
                            start_y: my,
                            orig: sel,
                        };
                        self.schedule_redraw(true);
                        return OverlayAction::None;
                    }
                }
            }

            self.stack.clear();
            self.commit_gen += 1;
            self.prev_active_dirty = None;
            self.pending_pen_dirty = None;
            self.active_tool = ToolKind::Select;
            self.selection = Some(PixelRect::from_points(mx, my, mx, my));
            self.drag = DragMode::NewSelection {
                anchor_x: mx,
                anchor_y: my,
            };
            self.schedule_redraw(true);
            OverlayAction::None
        } else {
            match self.drag {
                DragMode::DrawAnnotation => {
                    if let Some(ann) = self.stack.active.take() {
                        self.stack.push(ann);
                        self.commit_gen += 1;
                    }
                    self.pending_pen_dirty = None;
                }
                DragMode::NewSelection { .. } => {
                    if let Some(sel) = self.selection {
                        if sel.w < 4 || sel.h < 4 {
                            self.selection = None;
                        }
                    }
                }
                _ => {}
            }
            self.drag = DragMode::None;
            self.update_cursor(win_idx, mx, my);
            self.schedule_redraw(true);
            OverlayAction::None
        }
    }

    fn activate_toolbar_hit(&mut self, hit: ToolbarHit) -> OverlayAction {
        match hit {
            ToolbarHit::Tool(tool) => {
                self.active_tool = if self.active_tool == tool && tool != ToolKind::Select {
                    ToolKind::Select
                } else {
                    tool
                };
                self.schedule_redraw(false);
                OverlayAction::None
            }
            ToolbarHit::Color(idx) => {
                if idx < PALETTE.len() {
                    self.color_idx = idx;
                    if self.active_tool == ToolKind::Select || self.active_tool == ToolKind::Blur {
                        self.active_tool = ToolKind::Pen;
                    }
                    self.schedule_redraw(false);
                }
                OverlayAction::None
            }
            ToolbarHit::Thickness => {
                self.thickness_idx = (self.thickness_idx + 1) % STROKE_WIDTHS.len();
                self.schedule_redraw(false);
                OverlayAction::None
            }
            ToolbarHit::Undo => {
                if self.stack.undo() {
                    self.commit_gen += 1;
                    self.prev_active_dirty = None;
                    self.pending_pen_dirty = None;
                    self.schedule_redraw(true);
                }
                OverlayAction::None
            }
            ToolbarHit::Settings => OverlayAction::OpenSettings,
            ToolbarHit::Save => self.build_save_action(),
            ToolbarHit::Copy => OverlayAction::CopyAndClose,
            ToolbarHit::Cancel => OverlayAction::CancelAndClose,
        }
    }

    fn start_annotation(&mut self, x: f32, y: f32) {
        let color = PALETTE[self.color_idx].rgba;
        let width = STROKE_WIDTHS[self.thickness_idx];
        self.stack.active = Some(match self.active_tool {
            ToolKind::Pen => Annotation::Pen {
                points: vec![(x, y)],
                color,
                width,
            },
            ToolKind::Line => Annotation::Line {
                start: (x, y),
                end: (x, y),
                color,
                width,
            },
            ToolKind::Arrow => Annotation::Arrow {
                start: (x, y),
                end: (x, y),
                color,
                width,
            },
            ToolKind::Rect => Annotation::Rect {
                start: (x, y),
                end: (x, y),
                color,
                width,
            },
            ToolKind::Blur => Annotation::Blur {
                start: (x, y),
                end: (x, y),
                radius: 8 + self.thickness_idx * 5,
            },
            ToolKind::Select => return,
        });
    }

    fn on_cursor_moved(&mut self, win_idx: usize, mx: i32, my: i32) {
        self.mouse_pos = (mx, my);
        let max_w = self.capture.width as i32;
        let max_h = self.capture.height as i32;

        match self.drag {
            DragMode::None => {
                let prev_hover = self.hover_hit;
                self.hover_hit = self.hit_test_toolbar(mx, my);
                self.update_cursor(win_idx, mx, my);
                if prev_hover != self.hover_hit {
                    self.schedule_redraw(false);
                }
            }
            DragMode::NewSelection { anchor_x, anchor_y } => {
                let next =
                    Some(PixelRect::from_points(anchor_x, anchor_y, mx, my).clamped_to(max_w, max_h));
                if next != self.selection {
                    self.selection = next;
                    self.schedule_redraw(true);
                }
            }
            DragMode::MoveSelection { start_x, start_y, orig } => {
                let dx = mx - start_x;
                let dy = my - start_y;
                let nx = (orig.x + dx).clamp(0, (max_w - orig.w).max(0));
                let ny = (orig.y + dy).clamp(0, (max_h - orig.h).max(0));
                let next = Some(PixelRect {
                    x: nx,
                    y: ny,
                    w: orig.w,
                    h: orig.h,
                });
                if next != self.selection {
                    self.selection = next;
                    self.schedule_redraw(true);
                }
            }
            DragMode::ResizeSelection { handle, orig } => {
                let mut x0 = orig.x;
                let mut y0 = orig.y;
                let mut x1 = orig.right();
                let mut y1 = orig.bottom();
                match handle {
                    ResizeHandle::Nw => {
                        x0 = mx.min(x1 - 4);
                        y0 = my.min(y1 - 4);
                    }
                    ResizeHandle::N => y0 = my.min(y1 - 4),
                    ResizeHandle::Ne => {
                        x1 = mx.max(x0 + 4);
                        y0 = my.min(y1 - 4);
                    }
                    ResizeHandle::E => x1 = mx.max(x0 + 4),
                    ResizeHandle::Se => {
                        x1 = mx.max(x0 + 4);
                        y1 = my.max(y0 + 4);
                    }
                    ResizeHandle::S => y1 = my.max(y0 + 4),
                    ResizeHandle::Sw => {
                        x0 = mx.min(x1 - 4);
                        y1 = my.max(y0 + 4);
                    }
                    ResizeHandle::W => x0 = mx.min(x1 - 4),
                }
                let next =
                    Some(PixelRect::from_points(x0, y0, x1, y1).clamped_to(max_w, max_h));
                if next != self.selection {
                    self.selection = next;
                    self.schedule_redraw(true);
                }
            }
            DragMode::DrawAnnotation => {
                if let Some(sel) = self.selection {
                    let cx = (mx.clamp(sel.x, sel.right().saturating_sub(1))) as f32;
                    let cy = (my.clamp(sel.y, sel.bottom().saturating_sub(1))) as f32;
                    let mut changed = false;
                    let mut new_pen_seg: Option<((f32, f32), (f32, f32), [u8; 4], f32)> = None;

                    if let Some(ann) = self.stack.active.as_mut() {
                        match ann {
                            Annotation::Pen { points, color, width } => {
                                if let Some(&(lx, ly)) = points.last() {
                                    let dist2 = (cx - lx) * (cx - lx) + (cy - ly) * (cy - ly);
                                    if dist2 >= 4.0 {
                                        points.push((cx, cy));
                                        changed = true;
                                        new_pen_seg = Some(((lx, ly), (cx, cy), *color, *width));
                                    }
                                }
                            }
                            Annotation::Line { end, .. }
                            | Annotation::Arrow { end, .. }
                            | Annotation::Rect { end, .. }
                            | Annotation::Blur { end, .. } => {
                                if *end != (cx, cy) {
                                    *end = (cx, cy);
                                    changed = true;
                                }
                            }
                        }
                    }

                    if let Some((p0, p1, color, width)) = new_pen_seg {
                        let cache_matches = matches!(
                            &self.cached_commit,
                            Some((rev, rect, _)) if *rev == self.commit_gen && *rect == sel
                        );
                        if cache_matches {
                            if let Some(scratch) = self.live_scratch.as_mut() {
                                if scratch.dimensions() == (sel.w as u32, sel.h as u32) {
                                    draw_pen_segment(
                                        scratch,
                                        p0,
                                        p1,
                                        sel.x as f32,
                                        sel.y as f32,
                                        color,
                                        width,
                                    );
                                    let pad = width.ceil() as i32 + 4;
                                    let seg_rect = PixelRect::from_points(
                                        p0.0.min(p1.0).floor() as i32 - pad,
                                        p0.1.min(p1.1).floor() as i32 - pad,
                                        p0.0.max(p1.0).ceil() as i32 + pad,
                                        p0.1.max(p1.1).ceil() as i32 + pad,
                                    );
                                    self.pending_pen_dirty =
                                        union_rects(&[self.pending_pen_dirty, Some(seg_rect)]);
                                }
                            }
                        }
                    }

                    if changed {
                        self.schedule_redraw(true);
                    }
                }
            }
        }
    }

    fn update_cursor(&mut self, win_idx: usize, mx: i32, my: i32) {
        if self.hit_test_toolbar(mx, my).is_some() {
            self.set_cursor_cached(win_idx, CursorIcon::Pointer);
            return;
        }
        if let Some(sel) = self.selection {
            if let Some(handle) = hit_test_handles(sel, mx, my) {
                let icon = match handle {
                    ResizeHandle::Nw | ResizeHandle::Se => CursorIcon::NwseResize,
                    ResizeHandle::Ne | ResizeHandle::Sw => CursorIcon::NeswResize,
                    ResizeHandle::N | ResizeHandle::S => CursorIcon::NsResize,
                    ResizeHandle::E | ResizeHandle::W => CursorIcon::EwResize,
                };
                self.set_cursor_cached(win_idx, icon);
                return;
            }
            if sel.contains(mx, my) {
                if self.active_tool == ToolKind::Select {
                    self.set_cursor_cached(win_idx, CursorIcon::Move);
                } else {
                    self.set_cursor_cached(win_idx, CursorIcon::Crosshair);
                }
                return;
            }
        }
        self.set_cursor_cached(win_idx, CursorIcon::Crosshair);
    }

    fn hit_test_toolbar(&self, mx: i32, my: i32) -> Option<ToolbarHit> {
        let tb_rect = self.toolbar_rect?;
        let cached = self.cached_toolbar.as_ref()?;
        if !tb_rect.contains(mx, my) {
            return None;
        }
        let lx = mx - tb_rect.x;
        let ly = my - tb_rect.y;
        cached
            .buttons
            .iter()
            .find(|b| b.rect.contains(lx, ly))
            .map(|b| b.hit)
    }

    fn ensure_committed_cache(&mut self, sel: PixelRect) -> bool {
        let needs_rebuild = match &self.cached_commit {
            Some((rev, rect, _)) => *rev != self.commit_gen || *rect != sel,
            None => true,
        };
        if needs_rebuild {
            let img = composite_region(&self.capture, sel, &self.stack.items, None);
            self.cached_commit = Some((self.commit_gen, sel, img));
        }
        needs_rebuild
    }

    fn ensure_toolbar(&mut self) {
        let can_undo = !self.stack.is_empty();
        let needs_rebuild = match &self.cached_toolbar {
            Some(c) => {
                c.tool != self.active_tool
                    || c.color_idx != self.color_idx
                    || c.thickness_idx != self.thickness_idx
                    || c.can_undo != can_undo
                    || c.hover != self.hover_hit
            }
            None => true,
        };
        if needs_rebuild {
            self.cached_toolbar = Some(build_toolbar_pixmap(
                self.active_tool,
                self.color_idx,
                self.thickness_idx,
                can_undo,
                self.hover_hit,
            ));
        }
    }

    fn render(&mut self) {
        self.last_render = Instant::now();
        let prev_tb = self.toolbar_rect;
        let show_toolbar = self.selection.is_some_and(|s| s.w >= 4 && s.h >= 4)
            && !matches!(
                self.drag,
                DragMode::NewSelection { .. }
                    | DragMode::MoveSelection { .. }
                    | DragMode::ResizeSelection { .. }
            );

        if show_toolbar {
            self.ensure_toolbar();
            if let (Some(sel), Some(tb)) = (self.selection, &self.cached_toolbar) {
                self.toolbar_rect = Some(place_toolbar(
                    sel,
                    tb.pixmap.width() as i32,
                    tb.pixmap.height() as i32,
                    &self.capture.monitors,
                    self.capture.width as i32,
                    self.capture.height as i32,
                ));
            }
        } else {
            self.toolbar_rect = None;
        }

        let total_w = self.capture.width as i32;
        let total_h = self.capture.height as i32;

        let mut badge_rect: Option<PixelRect> = None;
        let mut hint_rect: Option<PixelRect> = None;
        if let Some(sel) = self.selection.filter(|s| s.w > 1 && s.h > 1) {
            let rebuild_badge =
                !matches!(&self.cached_badge, Some((bw, bh, _)) if *bw == sel.w && *bh == sel.h);
            if rebuild_badge {
                self.cached_badge = Some((sel.w, sel.h, build_dimension_badge(sel.w, sel.h)));
            }
            if let Some((_, _, badge)) = &self.cached_badge {
                let bw = badge.width() as i32;
                let bh = badge.height() as i32;
                let (bx, by) = if matches!(self.drag, DragMode::NewSelection { .. }) {
                    let mut px = self.mouse_pos.0 + 16;
                    let mut py = self.mouse_pos.1 + 18;
                    if px + bw > total_w - 8 {
                        px = self.mouse_pos.0 - bw - 12;
                    }
                    if py + bh > total_h - 8 {
                        py = self.mouse_pos.1 - bh - 12;
                    }
                    (px.max(4), py.max(4))
                } else {
                    let py = if sel.y - bh - 8 >= 4 {
                        sel.y - bh - 8
                    } else {
                        sel.y + 8
                    };
                    let px = sel.x.clamp(4, (total_w - bw - 4).max(4));
                    (px, py)
                };
                badge_rect = Some(PixelRect {
                    x: bx,
                    y: by,
                    w: bw,
                    h: bh,
                });
            }
        } else if matches!(self.drag, DragMode::None) {
            if self.cached_hint.is_none() {
                self.cached_hint = Some(build_hint_badge("DRAG TO SELECT  |  ESC CANCEL"));
            }
            if let Some(hint) = &self.cached_hint {
                let mon_bounds = self
                    .capture
                    .monitors
                    .first()
                    .map(|m| m.bounds)
                    .unwrap_or(PixelRect {
                        x: 0,
                        y: 0,
                        w: total_w,
                        h: total_h,
                    });
                let hw = hint.width() as i32;
                let hh = hint.height() as i32;
                hint_rect = Some(PixelRect {
                    x: mon_bounds.x + (mon_bounds.w - hw) / 2,
                    y: mon_bounds.y + 24,
                    w: hw,
                    h: hh,
                });
            }
        }

        let has_annotations = self
            .selection
            .is_some_and(|s| s.w > 0 && s.h > 0 && (!self.stack.items.is_empty() || self.stack.active.is_some()));

        let mut incremental_pen_rect: Option<PixelRect> = None;
        if has_annotations {
            let sel = self.selection.unwrap();
            let cache_rebuilt = self.ensure_committed_cache(sel);
            if let Some(active) = &self.stack.active {
                if let Some(pen_rect) = self.pending_pen_dirty.take() {
                    if !cache_rebuilt
                        && self.prev_selection == self.selection
                        && prev_tb == self.toolbar_rect
                    {
                        incremental_pen_rect = Some(pen_rect);
                    }
                }
                if incremental_pen_rect.is_none() {
                    let base_committed = &self.cached_commit.as_ref().unwrap().2;
                    let (bw, bh) = base_committed.dimensions();
                    let mut need_full_copy = cache_rebuilt;
                    let scratch = self.live_scratch.get_or_insert_with(|| {
                        need_full_copy = true;
                        RgbaImage::new(bw, bh)
                    });
                    if scratch.dimensions() != (bw, bh) {
                        *scratch = RgbaImage::new(bw, bh);
                        need_full_copy = true;
                    }
                    if need_full_copy {
                        scratch.as_mut().copy_from_slice(base_committed.as_raw());
                    } else if let Some(prev_dirty) = self.prev_active_dirty {
                        let local_dirty = PixelRect {
                            x: prev_dirty.x - sel.x,
                            y: prev_dirty.y - sel.y,
                            w: prev_dirty.w,
                            h: prev_dirty.h,
                        }
                        .clamped_to(bw as i32, bh as i32);
                        if local_dirty.w > 0 && local_dirty.h > 0 {
                            let stride = bw as usize * 4;
                            let row_bytes = local_dirty.w as usize * 4;
                            let src_raw = base_committed.as_raw();
                            let dst_raw = scratch.as_mut();
                            let x_off = local_dirty.x as usize * 4;
                            for row in local_dirty.y as usize..local_dirty.bottom() as usize {
                                let start = row * stride + x_off;
                                dst_raw[start..start + row_bytes]
                                    .copy_from_slice(&src_raw[start..start + row_bytes]);
                            }
                        }
                    }
                    apply_annotations_onto_with_scratch(
                        scratch,
                        sel.x as f32,
                        sel.y as f32,
                        &[],
                        Some(active),
                        &mut self.blur_scratch,
                    );
                }
            }
        } else {
            self.pending_pen_dirty = None;
        }

        let cur_active_dirty = self.stack.active.as_ref().map(annotation_bounds);

        self.damage_rects.clear();
        if let Some(pen_rect) = incremental_pen_rect {
            self.damage_rects.push(pen_rect);
        } else if !self.needs_scene_damage && prev_tb == self.toolbar_rect {
            if let Some(tb) = self.toolbar_rect {
                self.damage_rects.push(tb);
            }
        } else if matches!(self.drag, DragMode::DrawAnnotation)
            && self.prev_selection == self.selection
            && prev_tb == self.toolbar_rect
            && (self.prev_active_dirty.is_some() || cur_active_dirty.is_some())
        {
            if let Some(r) = union_rects(&[self.prev_active_dirty, cur_active_dirty]) {
                self.damage_rects.push(r);
            }
        } else {
            let cur_sel = self.selection.filter(|s| s.w > 0 && s.h > 0);
            let prev_sel = self.prev_selection.filter(|s| s.w > 0 && s.h > 0);

            match (prev_sel, cur_sel) {
                (Some(ps), Some(cs)) if !has_annotations => {
                    let outer_u =
                        union_rects(&[Some(expand_rect(ps, 4)), Some(expand_rect(cs, 4))])
                            .unwrap();
                    if (outer_u.w as i64) * (outer_u.h as i64) < 80_000 {
                        self.damage_rects.push(outer_u);
                    } else {
                        let kx0 = ps.x.max(cs.x) + 4;
                        let ky0 = ps.y.max(cs.y) + 4;
                        let kx1 = ps.right().min(cs.right()) - 4;
                        let ky1 = ps.bottom().min(cs.bottom()) - 4;
                        if kx0 < kx1 && ky0 < ky1 {
                            if outer_u.y < ky0 {
                                self.damage_rects.push(PixelRect {
                                    x: outer_u.x,
                                    y: outer_u.y,
                                    w: outer_u.w,
                                    h: ky0 - outer_u.y,
                                });
                            }
                            if ky1 < outer_u.bottom() {
                                self.damage_rects.push(PixelRect {
                                    x: outer_u.x,
                                    y: ky1,
                                    w: outer_u.w,
                                    h: outer_u.bottom() - ky1,
                                });
                            }
                            if outer_u.x < kx0 {
                                self.damage_rects.push(PixelRect {
                                    x: outer_u.x,
                                    y: ky0,
                                    w: kx0 - outer_u.x,
                                    h: ky1 - ky0,
                                });
                            }
                            if kx1 < outer_u.right() {
                                self.damage_rects.push(PixelRect {
                                    x: kx1,
                                    y: ky0,
                                    w: outer_u.right() - kx1,
                                    h: ky1 - ky0,
                                });
                            }
                        } else {
                            self.damage_rects.push(outer_u);
                        }
                    }
                }
                (ps, cs) => {
                    if let Some(u) = union_rects(&[
                        ps.map(|s| expand_rect(s, 4)),
                        cs.map(|s| expand_rect(s, 4)),
                    ]) {
                        self.damage_rects.push(u);
                    }
                }
            }

            if self.prev_badge_rect != badge_rect {
                if let Some(r) = self.prev_badge_rect {
                    self.damage_rects.push(r);
                }
                if let Some(r) = badge_rect {
                    self.damage_rects.push(r);
                }
            } else if let Some(br) = badge_rect {
                if self.damage_rects.iter().any(|d| d.intersects(br)) {
                    self.damage_rects.push(br);
                }
            }

            if self.prev_hint_rect != hint_rect {
                if let Some(r) = self.prev_hint_rect {
                    self.damage_rects.push(r);
                }
                if let Some(r) = hint_rect {
                    self.damage_rects.push(r);
                }
            }

            if prev_tb != self.toolbar_rect {
                if let Some(r) = prev_tb {
                    self.damage_rects.push(r);
                }
                if let Some(r) = self.toolbar_rect {
                    self.damage_rects.push(r);
                }
            }
        }

        self.damage_rects.retain_mut(|r| {
            *r = r.clamped_to(total_w, total_h);
            r.w > 0 && r.h > 0
        });

        let ann_img_opt: Option<&RgbaImage> = if has_annotations {
            if self.stack.active.is_some() {
                self.live_scratch.as_ref()
            } else {
                self.cached_commit.as_ref().map(|c| &c.2)
            }
        } else {
            None
        };

        for (m_idx, mw) in self.windows.iter_mut().enumerate() {
            let mon_cap = &self.capture.monitors[m_idx];
            let mon_bounds = mw.bounds;
            let cap_w = mon_bounds.w as usize;
            let cap_h = mon_bounds.h as usize;
            let buf_w = (mw.surface_w as usize).min(cap_w);
            let buf_h = (mw.surface_h as usize).min(cap_h);
            let buf_stride = mw.surface_w as usize;

            let force_full = mw.needs_full_redraw
                || !mw.has_painted_base
                || mw.last_rendered_size != (mw.surface_w, mw.surface_h);

            // Fast skip if this monitor has no intersecting damage rects and doesn't need full paint.
            if !force_full
                && !self
                    .damage_rects
                    .iter()
                    .any(|d| d.intersects(mon_bounds))
            {
                continue;
            }

            let Ok(mut buffer) = mw.surface.buffer_mut() else {
                continue;
            };
            let full_redraw = force_full || buffer.age() != 1;

            self.soft_rects.clear();
            if full_redraw {
                let full_local = PixelRect {
                    x: 0,
                    y: 0,
                    w: buf_w as i32,
                    h: buf_h as i32,
                };
                if full_local.w > 0 && full_local.h > 0 {
                    blit_local_damage(
                        &mut buffer,
                        buf_stride,
                        mon_cap,
                        mon_bounds,
                        full_local,
                        self.selection,
                        ann_img_opt,
                        badge_rect,
                        self.cached_badge.as_ref().map(|b| &b.2),
                        hint_rect,
                        self.cached_hint.as_ref(),
                        self.toolbar_rect,
                        self.cached_toolbar.as_ref().map(|t| &t.pixmap),
                    );
                }
                mw.has_painted_base = true;
                mw.needs_full_redraw = false;
                mw.last_rendered_size = (mw.surface_w, mw.surface_h);
                let _ = buffer.present();
            } else {
                for &dmg in &self.damage_rects {
                    let ix0 = dmg.x.max(mon_bounds.x);
                    let iy0 = dmg.y.max(mon_bounds.y);
                    let ix1 = dmg.right().min(mon_bounds.x + buf_w as i32);
                    let iy1 = dmg.bottom().min(mon_bounds.y + buf_h as i32);
                    if ix0 >= ix1 || iy0 >= iy1 {
                        continue;
                    }
                    let local_dmg = PixelRect {
                        x: ix0 - mon_bounds.x,
                        y: iy0 - mon_bounds.y,
                        w: ix1 - ix0,
                        h: iy1 - iy0,
                    };
                    blit_local_damage(
                        &mut buffer,
                        buf_stride,
                        mon_cap,
                        mon_bounds,
                        local_dmg,
                        self.selection,
                        ann_img_opt,
                        badge_rect,
                        self.cached_badge.as_ref().map(|b| &b.2),
                        hint_rect,
                        self.cached_hint.as_ref(),
                        self.toolbar_rect,
                        self.cached_toolbar.as_ref().map(|t| &t.pixmap),
                    );
                    self.soft_rects.push(softbuffer::Rect {
                        x: local_dmg.x as u32,
                        y: local_dmg.y as u32,
                        width: NonZeroU32::new(local_dmg.w as u32).unwrap(),
                        height: NonZeroU32::new(local_dmg.h as u32).unwrap(),
                    });
                }
                if !self.soft_rects.is_empty() {
                    let _ = buffer.present_with_damage(&self.soft_rects);
                }
            }
        }

        self.prev_selection = self.selection;
        self.prev_badge_rect = badge_rect;
        self.prev_hint_rect = hint_rect;
        self.prev_active_dirty = cur_active_dirty;
        self.needs_scene_damage = false;
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_local_damage(
    buffer: &mut [u32],
    buf_stride: usize,
    mon_cap: &CapturedMonitor,
    mon_bounds: PixelRect,
    local_dmg: PixelRect,
    selection: Option<PixelRect>,
    ann_img_opt: Option<&RgbaImage>,
    badge_rect: Option<PixelRect>,
    badge_pix: Option<&Pixmap>,
    hint_rect: Option<PixelRect>,
    hint_pix: Option<&Pixmap>,
    toolbar_rect: Option<PixelRect>,
    toolbar_pix: Option<&Pixmap>,
) {
    let cap_w = mon_bounds.w as usize;
    let dx0 = local_dmg.x as usize;
    let dy0 = local_dmg.y as usize;
    let dx1 = local_dmg.right() as usize;
    let dy1 = local_dmg.bottom() as usize;
    let src = &mon_cap.pixels_xrgb;

    let local_sel = selection
        .filter(|s| s.w > 0 && s.h > 0)
        .map(|s| PixelRect {
            x: s.x - mon_bounds.x,
            y: s.y - mon_bounds.y,
            w: s.w,
            h: s.h,
        });

    match local_sel {
        None => {
            for y in dy0..dy1 {
                let buf_off = y * buf_stride;
                let cap_off = y * cap_w;
                dim_xrgb_slice(
                    &mut buffer[buf_off + dx0..buf_off + dx1],
                    &src[cap_off + dx0..cap_off + dx1],
                );
            }
        }
        Some(lsel) => {
            let sx0 = lsel.x.clamp(0, cap_w as i32) as usize;
            let sy0 = lsel.y.clamp(0, mon_bounds.h) as usize;
            let sx1 = lsel.right().clamp(0, cap_w as i32) as usize;
            let sy1 = lsel.bottom().clamp(0, mon_bounds.h) as usize;
            let global_sel = selection.unwrap();

            for y in dy0..dy1 {
                let buf_off = y * buf_stride;
                let cap_off = y * cap_w;
                if y < sy0 || y >= sy1 {
                    dim_xrgb_slice(
                        &mut buffer[buf_off + dx0..buf_off + dx1],
                        &src[cap_off + dx0..cap_off + dx1],
                    );
                    continue;
                }

                let left_end = dx1.min(sx0);
                if dx0 < left_end {
                    dim_xrgb_slice(
                        &mut buffer[buf_off + dx0..buf_off + left_end],
                        &src[cap_off + dx0..cap_off + left_end],
                    );
                }

                let mid_start = dx0.max(sx0);
                let mid_end = dx1.min(sx1);
                if mid_start < mid_end {
                    if let Some(ann_img) = ann_img_opt {
                        let abs_y = mon_bounds.y + y as i32;
                        let ay = (abs_y - global_sel.y) as usize;
                        let ann_w = ann_img.width() as usize;
                        let ann_h = ann_img.height() as usize;
                        if ay < ann_h {
                            let raw = ann_img.as_raw();
                            let row_base = ay * ann_w * 4;
                            let abs_x_start = mon_bounds.x + mid_start as i32;
                            let ax_start = (abs_x_start - global_sel.x) as usize;
                            let ax_end = ax_start + (mid_end - mid_start);
                            if ax_end <= ann_w {
                                orig_rgba_to_xrgb(
                                    &mut buffer[buf_off + mid_start..buf_off + mid_end],
                                    &raw[row_base + ax_start * 4..row_base + ax_end * 4],
                                );
                            }
                        }
                    } else {
                        buffer[buf_off + mid_start..buf_off + mid_end]
                            .copy_from_slice(&src[cap_off + mid_start..cap_off + mid_end]);
                    }
                }

                let right_start = dx0.max(sx1);
                if right_start < dx1 {
                    dim_xrgb_slice(
                        &mut buffer[buf_off + right_start..buf_off + dx1],
                        &src[cap_off + right_start..cap_off + dx1],
                    );
                }
            }
        }
    }

    if let Some(lsel) = local_sel.filter(|s| s.w > 1 && s.h > 1) {
        draw_selection_chrome_clipped(
            buffer,
            buf_stride as i32,
            mon_bounds.h,
            lsel,
            local_dmg,
        );
        if let (Some(br), Some(badge)) = (badge_rect, badge_pix) {
            blit_pixmap_clipped(
                buffer,
                buf_stride as i32,
                mon_bounds.h,
                badge,
                br.x - mon_bounds.x,
                br.y - mon_bounds.y,
                local_dmg.x,
                local_dmg.y,
                local_dmg.right(),
                local_dmg.bottom(),
            );
        }
    } else if let (Some(hr), Some(hint)) = (hint_rect, hint_pix) {
        blit_pixmap_clipped(
            buffer,
            buf_stride as i32,
            mon_bounds.h,
            hint,
            hr.x - mon_bounds.x,
            hr.y - mon_bounds.y,
            local_dmg.x,
            local_dmg.y,
            local_dmg.right(),
            local_dmg.bottom(),
        );
    }

    if let (Some(tb_rect), Some(tb)) = (toolbar_rect, toolbar_pix) {
        blit_pixmap_clipped(
            buffer,
            buf_stride as i32,
            mon_bounds.h,
            tb,
            tb_rect.x - mon_bounds.x,
            tb_rect.y - mon_bounds.y,
            local_dmg.x,
            local_dmg.y,
            local_dmg.right(),
            local_dmg.bottom(),
        );
    }
}

#[inline]
fn dim_xrgb_slice(dst: &mut [u32], src: &[u32]) {
    for (d, &px) in dst.iter_mut().zip(src.iter()) {
        *d = ((px & 0x00FE_FEFE) >> 1) + ((px & 0x00F0_F0F0) >> 4);
    }
}

#[inline]
fn orig_rgba_to_xrgb(dst: &mut [u32], src_rgba: &[u8]) {
    for (d, rgba) in dst.iter_mut().zip(src_rgba.chunks_exact(4)) {
        *d = ((rgba[0] as u32) << 16) | ((rgba[1] as u32) << 8) | (rgba[2] as u32);
    }
}

fn expand_rect(r: PixelRect, pad: i32) -> PixelRect {
    PixelRect {
        x: r.x - pad,
        y: r.y - pad,
        w: r.w + pad * 2,
        h: r.h + pad * 2,
    }
}

fn annotation_bounds(ann: &Annotation) -> PixelRect {
    match ann {
        Annotation::Pen { points, width, .. } => {
            let mut min_x = f32::MAX;
            let mut min_y = f32::MAX;
            let mut max_x = f32::MIN;
            let mut max_y = f32::MIN;
            for &(x, y) in points {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
            let pad = (*width).ceil() as i32 + 4;
            PixelRect::from_points(
                min_x.floor() as i32 - pad,
                min_y.floor() as i32 - pad,
                max_x.ceil() as i32 + pad,
                max_y.ceil() as i32 + pad,
            )
        }
        Annotation::Line { start, end, width, .. }
        | Annotation::Rect { start, end, width, .. } => {
            let pad = (*width).ceil() as i32 + 4;
            PixelRect::from_points(
                start.0.min(end.0).floor() as i32 - pad,
                start.1.min(end.1).floor() as i32 - pad,
                start.0.max(end.0).ceil() as i32 + pad,
                start.1.max(end.1).ceil() as i32 + pad,
            )
        }
        Annotation::Arrow { start, end, width, .. } => {
            let pad = (*width * 4.0).ceil() as i32 + 12;
            PixelRect::from_points(
                start.0.min(end.0).floor() as i32 - pad,
                start.1.min(end.1).floor() as i32 - pad,
                start.0.max(end.0).ceil() as i32 + pad,
                start.1.max(end.1).ceil() as i32 + pad,
            )
        }
        Annotation::Blur { start, end, .. } => PixelRect::from_points(
            start.0.min(end.0).floor() as i32 - 2,
            start.1.min(end.1).floor() as i32 - 2,
            start.0.max(end.0).ceil() as i32 + 2,
            start.1.max(end.1).ceil() as i32 + 2,
        ),
    }
}

fn union_rects(rects: &[Option<PixelRect>]) -> Option<PixelRect> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    let mut any = false;
    for r in rects.iter().flatten() {
        if r.w <= 0 || r.h <= 0 {
            continue;
        }
        any = true;
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_x = max_x.max(r.right());
        max_y = max_y.max(r.bottom());
    }
    if any {
        Some(PixelRect {
            x: min_x,
            y: min_y,
            w: max_x - min_x,
            h: max_y - min_y,
        })
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn disable_window_animations(window: &Window) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            dw_attribute: u32,
            pv_attribute: *const i32,
            cb_attribute: u32,
        ) -> i32;
    }

    const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(win32) = handle.as_raw() {
            let disable: i32 = 1;
            unsafe {
                DwmSetWindowAttribute(
                    win32.hwnd.get(),
                    DWMWA_TRANSITIONS_FORCEDISABLED,
                    &disable,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn disable_window_animations(_window: &Window) {}

fn hit_test_handles(sel: PixelRect, mx: i32, my: i32) -> Option<ResizeHandle> {
    if sel.w < 4 || sel.h < 4 {
        return None;
    }
    let cx = sel.x + sel.w / 2;
    let cy = sel.y + sel.h / 2;
    let rx = sel.right();
    let by = sel.bottom();
    let r = 8;

    let near = |px: i32, py: i32| (mx - px).abs() <= r && (my - py).abs() <= r;

    if near(sel.x, sel.y) {
        Some(ResizeHandle::Nw)
    } else if near(rx, sel.y) {
        Some(ResizeHandle::Ne)
    } else if near(rx, by) {
        Some(ResizeHandle::Se)
    } else if near(sel.x, by) {
        Some(ResizeHandle::Sw)
    } else if near(cx, sel.y) {
        Some(ResizeHandle::N)
    } else if near(rx, cy) {
        Some(ResizeHandle::E)
    } else if near(cx, by) {
        Some(ResizeHandle::S)
    } else if near(sel.x, cy) {
        Some(ResizeHandle::W)
    } else {
        None
    }
}

fn place_toolbar(
    sel: PixelRect,
    tb_w: i32,
    tb_h: i32,
    monitors: &[CapturedMonitor],
    total_w: i32,
    total_h: i32,
) -> PixelRect {
    let anchor_x = sel.right() - 1;
    let anchor_y = sel.bottom() - 1;
    let bounds = monitors
        .iter()
        .find(|m| m.bounds.contains(anchor_x, anchor_y))
        .map(|m| m.bounds)
        .unwrap_or(PixelRect {
            x: 0,
            y: 0,
            w: total_w,
            h: total_h,
        });

    let pad = 8;
    let x = (sel.right() - tb_w)
        .clamp(bounds.x + pad, (bounds.right() - tb_w - pad).max(bounds.x + pad));

    let below_y = sel.bottom() + 10;
    let above_y = sel.y - tb_h - 10;
    let y = if below_y + tb_h + pad <= bounds.bottom() {
        below_y
    } else if above_y >= bounds.y + pad {
        above_y
    } else {
        (sel.bottom() - tb_h - 10).clamp(
            bounds.y + pad,
            (bounds.bottom() - tb_h - pad).max(bounds.y + pad),
        )
    };

    PixelRect {
        x,
        y,
        w: tb_w,
        h: tb_h,
    }
}

fn draw_selection_chrome_clipped(
    buf: &mut [u32],
    buf_stride: i32,
    buf_h: i32,
    sel: PixelRect,
    clip: PixelRect,
) {
    if sel.right() <= 0 || sel.bottom() <= 0 || sel.x >= buf_stride || sel.y >= buf_h {
        return;
    }
    let x0 = sel.x;
    let y0 = sel.y;
    let x1 = sel.right() - 1;
    let y1 = sel.bottom() - 1;
    let w = buf_stride as usize;

    let cx0 = clip.x.max(0);
    let cy0 = clip.y.max(0);
    let cx1 = clip.right().min(buf_stride);
    let cy1 = clip.bottom().min(buf_h);

    let border_col = 0x00_38BDF8;
    let h_min = x0.max(cx0);
    let h_max = (x1 + 1).min(cx1);
    if h_min < h_max {
        if y0 >= cy0 && y0 < cy1 {
            for x in h_min..h_max {
                buf[y0 as usize * w + x as usize] = border_col;
            }
        }
        if y1 >= cy0 && y1 < cy1 {
            for x in h_min..h_max {
                buf[y1 as usize * w + x as usize] = border_col;
            }
        }
    }

    let v_min = y0.max(cy0);
    let v_max = (y1 + 1).min(cy1);
    if v_min < v_max {
        if x0 >= cx0 && x0 < cx1 {
            for y in v_min..v_max {
                buf[y as usize * w + x0 as usize] = border_col;
            }
        }
        if x1 >= cx0 && x1 < cx1 {
            for y in v_min..v_max {
                buf[y as usize * w + x1 as usize] = border_col;
            }
        }
    }

    let cx = (x0 + x1) / 2;
    let cy = (y0 + y1) / 2;
    let points = [
        (x0, y0),
        (cx, y0),
        (x1, y0),
        (x1, cy),
        (x1, y1),
        (cx, y1),
        (x0, y1),
        (x0, cy),
    ];

    for (hx, hy) in points {
        if hx + 3 < cx0 || hx - 3 >= cx1 || hy + 3 < cy0 || hy - 3 >= cy1 {
            continue;
        }
        for dy in -3..=3 {
            let py = hy + dy;
            if py < cy0 || py >= cy1 {
                continue;
            }
            for dx in -3..=3 {
                let px = hx + dx;
                if px < cx0 || px >= cx1 {
                    continue;
                }
                let is_edge = dx == -3 || dx == 3 || dy == -3 || dy == 3;
                buf[py as usize * w + px as usize] =
                    if is_edge { 0x00_0F172A } else { 0x00_FFFFFF };
            }
        }
    }
}

fn build_dimension_badge(w: i32, h: i32) -> Pixmap {
    let label = format!("{w} x {h} px");
    build_pill_text(&label)
}

fn build_hint_badge(text: &str) -> Pixmap {
    build_pill_text(text)
}

fn build_pill_text(text: &str) -> Pixmap {
    let scale = 2i32;
    let char_adv = 6 * scale;
    let pad_x = 10;
    let pad_y = 6;
    let pw = (text.len() as i32 * char_adv + pad_x * 2).max(32) as u32;
    let ph = (7 * scale + pad_y * 2) as u32;
    let mut pix = Pixmap::new(pw, ph).unwrap();

    draw_rounded_rect(&mut pix, 0.5, 0.5, pw as f32 - 1.0, ph as f32 - 1.0, 6.0, [20, 22, 28, 225]);
    stroke_rounded_rect(&mut pix, 0.5, 0.5, pw as f32 - 1.0, ph as f32 - 1.0, 6.0, [71, 85, 105, 220], 1.0);

    draw_bitmap_text(&mut pix, text, pad_x, pad_y, scale, [241, 245, 249, 255]);
    pix
}

fn build_toolbar_pixmap(
    active_tool: ToolKind,
    color_idx: usize,
    thickness_idx: usize,
    can_undo: bool,
    hover: Option<ToolbarHit>,
) -> CachedToolbar {
    let h = 42i32;
    let btn_s = 30i32;
    let gap = 4i32;
    let swatch_s = 22i32;
    let mut x = 8i32;
    let by = (h - btn_s) / 2;
    let mut buttons = Vec::with_capacity(16);

    let tools = [
        ToolKind::Select,
        ToolKind::Pen,
        ToolKind::Line,
        ToolKind::Arrow,
        ToolKind::Rect,
        ToolKind::Blur,
    ];
    for t in tools {
        buttons.push(ToolbarButton {
            rect: PixelRect { x, y: by, w: btn_s, h: btn_s },
            hit: ToolbarHit::Tool(t),
        });
        x += btn_s + gap;
    }

    let div1_x = x + 2;
    x += 9;

    let sy = (h - swatch_s) / 2;
    for i in 0..PALETTE.len() {
        buttons.push(ToolbarButton {
            rect: PixelRect { x, y: sy, w: swatch_s, h: swatch_s },
            hit: ToolbarHit::Color(i),
        });
        x += swatch_s + 3;
    }
    x += 2;

    buttons.push(ToolbarButton {
        rect: PixelRect { x, y: by, w: btn_s, h: btn_s },
        hit: ToolbarHit::Thickness,
    });
    x += btn_s + gap;

    buttons.push(ToolbarButton {
        rect: PixelRect { x, y: by, w: btn_s, h: btn_s },
        hit: ToolbarHit::Undo,
    });
    x += btn_s + gap;

    buttons.push(ToolbarButton {
        rect: PixelRect { x, y: by, w: btn_s, h: btn_s },
        hit: ToolbarHit::Settings,
    });
    x += btn_s + gap;

    let div2_x = x + 2;
    x += 9;

    buttons.push(ToolbarButton {
        rect: PixelRect { x, y: by, w: btn_s, h: btn_s },
        hit: ToolbarHit::Save,
    });
    x += btn_s + gap;

    buttons.push(ToolbarButton {
        rect: PixelRect { x, y: by, w: btn_s + 6, h: btn_s },
        hit: ToolbarHit::Copy,
    });
    x += btn_s + 6 + gap;

    buttons.push(ToolbarButton {
        rect: PixelRect { x, y: by, w: btn_s, h: btn_s },
        hit: ToolbarHit::Cancel,
    });
    x += btn_s + 8;

    let total_w = x as u32;
    let mut pix = Pixmap::new(total_w, h as u32).unwrap();

    draw_rounded_rect(&mut pix, 0.5, 0.5, total_w as f32 - 1.0, h as f32 - 1.0, 9.0, [24, 24, 27, 236]);
    stroke_rounded_rect(&mut pix, 0.5, 0.5, total_w as f32 - 1.0, h as f32 - 1.0, 9.0, [63, 63, 70, 240], 1.0);

    fill_solid_rect(&mut pix, div1_x as f32, 9.0, 1.0, (h - 18) as f32, [63, 63, 70, 220]);
    fill_solid_rect(&mut pix, div2_x as f32, 9.0, 1.0, (h - 18) as f32, [63, 63, 70, 220]);

    for btn in &buttons {
        let r = btn.rect;
        let is_hover = hover == Some(btn.hit);
        match btn.hit {
            ToolbarHit::Tool(t) => {
                let is_active = active_tool == t;
                if is_active {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [37, 99, 235, 240]);
                } else if is_hover {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [55, 65, 81, 200]);
                }
                draw_tool_icon(&mut pix, t, r, [248, 250, 252, 255]);
            }
            ToolbarHit::Color(idx) => {
                let cx = r.x as f32 + r.w as f32 * 0.5;
                let cy = r.y as f32 + r.h as f32 * 0.5;
                if idx == color_idx {
                    draw_circle(&mut pix, cx, cy, 9.5, [255, 255, 255, 255]);
                    draw_circle(&mut pix, cx, cy, 7.6, [24, 24, 27, 255]);
                } else if is_hover {
                    draw_circle(&mut pix, cx, cy, 8.8, [161, 161, 170, 220]);
                }
                draw_circle(&mut pix, cx, cy, 6.2, PALETTE[idx].rgba);
            }
            ToolbarHit::Thickness => {
                if is_hover {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [55, 65, 81, 200]);
                }
                let radius = match thickness_idx {
                    0 => 2.3,
                    1 => 4.0,
                    _ => 6.0,
                };
                draw_circle(
                    &mut pix,
                    r.x as f32 + r.w as f32 * 0.5,
                    r.y as f32 + r.h as f32 * 0.5,
                    radius,
                    PALETTE[color_idx].rgba,
                );
            }
            ToolbarHit::Undo => {
                if is_hover && can_undo {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [55, 65, 81, 200]);
                }
                let col = if can_undo { [248, 250, 252, 255] } else { [100, 116, 139, 140] };
                draw_undo_icon(&mut pix, r, col);
            }
            ToolbarHit::Settings => {
                if is_hover {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [55, 65, 81, 200]);
                }
                draw_gear_icon(&mut pix, r, [248, 250, 252, 255]);
            }
            ToolbarHit::Save => {
                if is_hover {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [55, 65, 81, 200]);
                }
                draw_save_icon(&mut pix, r, [248, 250, 252, 255]);
            }
            ToolbarHit::Copy => {
                let bg = if is_hover { [22, 163, 74, 255] } else { [34, 197, 94, 235] };
                draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, bg);
                draw_copy_icon(&mut pix, r, [255, 255, 255, 255]);
            }
            ToolbarHit::Cancel => {
                if is_hover {
                    draw_rounded_rect(&mut pix, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, [220, 38, 38, 220]);
                }
                draw_close_icon(&mut pix, r, [248, 250, 252, 255]);
            }
        }
    }

    CachedToolbar {
        pixmap: pix,
        buttons,
        tool: active_tool,
        color_idx,
        thickness_idx,
        can_undo,
        hover,
    }
}

fn draw_circle(pix: &mut Pixmap, cx: f32, cy: f32, r: f32, col: [u8; 4]) {
    if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
        pix.fill_path(&path, &paint_rgba(col), FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke_segs(pix: &mut Pixmap, pts: &[(f32, f32)], col: [u8; 4], width: f32, close: bool) {
    if pts.len() < 2 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    for &(x, y) in &pts[1..] {
        pb.line_to(x, y);
    }
    if close {
        pb.close();
    }
    if let Some(path) = pb.finish() {
        let stroke = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pix.stroke_path(&path, &paint_rgba(col), &stroke, Transform::identity(), None);
    }
}

fn draw_tool_icon(pix: &mut Pixmap, tool: ToolKind, r: PixelRect, col: [u8; 4]) {
    let x = r.x as f32;
    let y = r.y as f32;
    let w = r.w as f32;
    let h = r.h as f32;
    match tool {
        ToolKind::Select => {
            let mut pb = PathBuilder::new();
            pb.move_to(x + 9.0, y + 7.0);
            pb.line_to(x + 9.0, y + 22.0);
            pb.line_to(x + 13.5, y + 18.0);
            pb.line_to(x + 17.0, y + 23.5);
            pb.line_to(x + 19.2, y + 22.2);
            pb.line_to(x + 15.8, y + 16.8);
            pb.line_to(x + 21.0, y + 16.5);
            pb.close();
            if let Some(p) = pb.finish() {
                pix.fill_path(&p, &paint_rgba(col), FillRule::Winding, Transform::identity(), None);
            }
        }
        ToolKind::Pen => {
            let mut pb = PathBuilder::new();
            pb.move_to(x + 7.0, y + 21.0);
            pb.quad_to(x + 11.0, y + 8.0, x + 15.0, y + 15.0);
            pb.quad_to(x + 19.0, y + 22.0, x + 23.0, y + 9.0);
            if let Some(p) = pb.finish() {
                let stroke = Stroke {
                    width: 2.0,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                };
                pix.stroke_path(&p, &paint_rgba(col), &stroke, Transform::identity(), None);
            }
        }
        ToolKind::Line => {
            stroke_segs(pix, &[(x + 8.0, y + 22.0), (x + 22.0, y + 8.0)], col, 2.2, false);
        }
        ToolKind::Arrow => {
            stroke_segs(pix, &[(x + 8.0, y + 22.0), (x + 21.0, y + 9.0)], col, 2.0, false);
            stroke_segs(
                pix,
                &[(x + 14.0, y + 8.5), (x + 21.5, y + 8.5), (x + 21.5, y + 16.0)],
                col,
                2.0,
                false,
            );
        }
        ToolKind::Rect => {
            if let Some(rc) = Rect::from_xywh(x + 7.5, y + 8.5, w - 15.0, h - 17.0) {
                let path = PathBuilder::from_rect(rc);
                let stroke = Stroke {
                    width: 2.0,
                    line_join: LineJoin::Round,
                    ..Stroke::default()
                };
                pix.stroke_path(&path, &paint_rgba(col), &stroke, Transform::identity(), None);
            }
        }
        ToolKind::Blur => {
            for row in 0..3 {
                for c in 0..3 {
                    let alpha = if (row + c) % 2 == 0 { 245 } else { 115 };
                    fill_solid_rect(
                        pix,
                        x + 7.5 + c as f32 * 5.2,
                        y + 7.5 + row as f32 * 5.2,
                        4.2,
                        4.2,
                        [col[0], col[1], col[2], alpha],
                    );
                }
            }
        }
    }
}

fn draw_undo_icon(pix: &mut Pixmap, r: PixelRect, col: [u8; 4]) {
    let x = r.x as f32;
    let y = r.y as f32;
    let mut pb = PathBuilder::new();
    pb.move_to(x + 9.0, y + 13.0);
    pb.line_to(x + 17.0, y + 13.0);
    pb.quad_to(x + 22.5, y + 13.0, x + 22.5, y + 18.0);
    pb.quad_to(x + 22.5, y + 22.0, x + 17.0, y + 22.0);
    pb.line_to(x + 12.0, y + 22.0);
    if let Some(p) = pb.finish() {
        let stroke = Stroke {
            width: 2.0,
            line_cap: LineCap::Round,
            ..Stroke::default()
        };
        pix.stroke_path(&p, &paint_rgba(col), &stroke, Transform::identity(), None);
    }
    stroke_segs(
        pix,
        &[(x + 12.5, y + 9.0), (x + 8.5, y + 13.0), (x + 12.5, y + 17.0)],
        col,
        2.0,
        false,
    );
}

fn draw_save_icon(pix: &mut Pixmap, r: PixelRect, col: [u8; 4]) {
    let x = r.x as f32;
    let y = r.y as f32;
    stroke_segs(pix, &[(x + 15.0, y + 7.5), (x + 15.0, y + 17.5)], col, 2.0, false);
    stroke_segs(
        pix,
        &[(x + 11.0, y + 14.0), (x + 15.0, y + 18.0), (x + 19.0, y + 14.0)],
        col,
        2.0,
        false,
    );
    stroke_segs(
        pix,
        &[
            (x + 8.0, y + 18.5),
            (x + 8.0, y + 22.0),
            (x + 22.0, y + 22.0),
            (x + 22.0, y + 18.5),
        ],
        col,
        2.0,
        false,
    );
}

fn draw_copy_icon(pix: &mut Pixmap, r: PixelRect, col: [u8; 4]) {
    let x = r.x as f32 + 3.0;
    let y = r.y as f32;
    stroke_segs(
        pix,
        &[(x + 8.0, y + 15.5), (x + 13.0, y + 20.5), (x + 22.0, y + 10.0)],
        col,
        2.4,
        false,
    );
}

fn draw_close_icon(pix: &mut Pixmap, r: PixelRect, col: [u8; 4]) {
    let x = r.x as f32;
    let y = r.y as f32;
    stroke_segs(pix, &[(x + 9.5, y + 9.5), (x + 20.5, y + 20.5)], col, 2.0, false);
    stroke_segs(pix, &[(x + 20.5, y + 9.5), (x + 9.5, y + 20.5)], col, 2.0, false);
}

fn draw_gear_icon(pix: &mut Pixmap, r: PixelRect, col: [u8; 4]) {
    let cx = r.x as f32 + r.w as f32 * 0.5;
    let cy = r.y as f32 + r.h as f32 * 0.5;
    if let Some(ring) = PathBuilder::from_circle(cx, cy, 4.6) {
        let stroke = Stroke {
            width: 2.0,
            ..Stroke::default()
        };
        pix.stroke_path(&ring, &paint_rgba(col), &stroke, Transform::identity(), None);
    }
    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::FRAC_PI_4;
        let (sin, cos) = angle.sin_cos();
        stroke_segs(
            pix,
            &[
                (cx + cos * 5.6, cy + sin * 5.6),
                (cx + cos * 7.6, cy + sin * 7.6),
            ],
            col,
            2.0,
            false,
        );
    }
}
