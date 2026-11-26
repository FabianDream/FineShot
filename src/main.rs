#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod capture;
mod config;
mod export;
mod overlay;
mod settings;
mod tools;
mod ui;
mod updater;

use std::sync::Arc;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton as TrayMouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::capture::VirtualCapture;
use crate::config::{AppConfig, format_hotkey_label};
use crate::overlay::{OverlayAction, OverlaySession};
use crate::settings::{SettingsAction, SettingsWindow};

#[derive(Debug)]
enum UserEvent {
    HotKey(GlobalHotKeyEvent),
    TrayClick,
    MenuAction(MenuId),
    SaveFinished(Result<bool, String>),
    QuickExportDone,
    UpdateAvailable(updater::UpdateInfo),
    UpdateNotFound {
        is_manual: bool,
    },
    UpdateCheckFailed {
        is_manual: bool,
        err: String,
    },
    UpdateDownloaded(std::path::PathBuf),
    UpdateFailed(String),
}

struct TrayState {
    tray: TrayIcon,
    hotkey_manager: GlobalHotKeyManager,
    take_id: MenuId,
    settings_id: MenuId,
    updates_id: MenuId,
    quit_id: MenuId,
}

struct App {
    instant: bool,
    open_settings_on_start: bool,
    config: AppConfig,
    pending_capture: Option<VirtualCapture>,
    overlay: Option<OverlaySession>,
    settings: Option<SettingsWindow>,
    tray: Option<TrayState>,
    proxy: EventLoopProxy<UserEvent>,
    active_hotkey: Option<HotKey>,
    hotkey_error: Option<String>,
    update_status: updater::UpdateStatus,
    startup_update_checked: bool,
}

impl App {
    fn open_overlay(&mut self, event_loop: &ActiveEventLoop, cap: VirtualCapture) {
        if self.overlay.is_some() {
            return;
        }
        match OverlaySession::create(event_loop, cap) {
            Ok(session) => {
                self.overlay = Some(session);
            }
            Err(err) => {
                eprintln!("Overlay error: {err}");
                if self.instant && self.settings.is_none() {
                    event_loop.exit();
                }
            }
        }
    }

    fn handle_captured(&mut self, event_loop: &ActiveEventLoop, cap: VirtualCapture) {
        self.open_overlay(event_loop, cap);
    }

    fn capture_and_open(&mut self, event_loop: &ActiveEventLoop) {
        if self.overlay.is_some() {
            return;
        }
        match capture::capture_virtual_desktop() {
            Ok(cap) => self.handle_captured(event_loop, cap),
            Err(err) => eprintln!("Capture failed: {err}"),
        }
    }

    fn open_settings(&mut self, event_loop: &ActiveEventLoop) {
        let autostart_enabled = autostart::is_enabled();
        if let Some(win) = &mut self.settings {
            win.set_autostart(autostart_enabled);
            win.mark_dirty_and_redraw();
            win.focus();
            return;
        }
        let hk_ok = self.active_hotkey.is_some() && self.hotkey_error.is_none();
        match SettingsWindow::create(
            event_loop,
            &self.config,
            hk_ok,
            self.hotkey_error.as_deref(),
            autostart_enabled,
        ) {
            Ok(win) => {
                self.settings = Some(win);
            }
            Err(err) => eprintln!("Settings window error: {err}"),
        }
    }

    fn init_tray(&mut self) {
        if self.tray.is_some() {
            return;
        }

        let take_item = MenuItem::new("Take Screenshot", true, None);
        let settings_item = MenuItem::new("Settings...", true, None);
        let updates_item = MenuItem::new("Check for Updates...", true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let take_id = take_item.id().clone();
        let settings_id = settings_item.id().clone();
        let updates_id = updates_item.id().clone();
        let quit_id = quit_item.id().clone();

        let menu = Menu::new();
        let _ = menu.append(&take_item);
        let _ = menu.append(&settings_item);
        let _ = menu.append(&updates_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit_item);

        let hotkey_manager = match GlobalHotKeyManager::new() {
            Ok(mgr) => mgr,
            Err(err) => {
                eprintln!("Failed to initialize global hotkey manager: {err}");
                self.hotkey_error = Some("Hotkey manager failed".to_string());
                return;
            }
        };

        let configured_hk = self.config.parsed_hotkey();
        let tooltip = if hotkey_manager.register(configured_hk).is_ok() {
            self.active_hotkey = Some(configured_hk);
            self.hotkey_error = None;
            format!("FineShot ({})", format_hotkey_label(configured_hk))
        } else {
            self.hotkey_error = Some(format!(
                "{} is busy in OS - pick another key",
                format_hotkey_label(configured_hk)
            ));
            "FineShot (Hotkey busy - open Settings)".to_string()
        };

        let icon = build_tray_icon();
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(&tooltip)
            .with_icon(icon)
            .build()
        {
            Ok(t) => t,
            Err(err) => {
                eprintln!("Failed to build tray icon: {err}");
                return;
            }
        };

        let hotkey_proxy = self.proxy.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            let _ = hotkey_proxy.send_event(UserEvent::HotKey(event));
        }));

        let menu_proxy = self.proxy.clone();
        MenuEvent::set_event_handler(Some(Arc::new(move |event: MenuEvent| {
            let _ = menu_proxy.send_event(UserEvent::MenuAction(event.id));
        })));

        let tray_proxy = self.proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: TrayMouseButton::Left,
                ..
            } = event
            {
                let _ = tray_proxy.send_event(UserEvent::TrayClick);
            }
        }));

        self.tray = Some(TrayState {
            tray,
            hotkey_manager,
            take_id,
            settings_id,
            updates_id,
            quit_id,
        });
        trim_working_set();
        if !self.startup_update_checked {
            self.startup_update_checked = true;
            spawn_update_check(self.proxy.clone(), false, 800);
        }
    }

    fn trigger_update_check(&mut self, is_manual: bool) {
        if matches!(
            self.update_status,
            updater::UpdateStatus::Checking | updater::UpdateStatus::Downloading
        ) {
            return;
        }
        self.update_status = updater::UpdateStatus::Checking;
        if is_manual {
            if let Some(s) = &mut self.settings {
                s.set_status(true, "Checking GitHub for updates...");
            }
        }
        spawn_update_check(self.proxy.clone(), is_manual, 0);
    }

    fn apply_new_hotkey(&mut self, new_hk: HotKey) {
        if let Some(tray) = &self.tray {
            if let Some(old_hk) = self.active_hotkey.take() {
                let _ = tray.hotkey_manager.unregister(old_hk);
            }
            if tray.hotkey_manager.register(new_hk).is_ok() {
                self.active_hotkey = Some(new_hk);
                self.hotkey_error = None;
                self.config.hotkey = new_hk.to_string();
                let _ = self.config.save();
                let label = format_hotkey_label(new_hk);
                let _ = tray.tray.set_tooltip(Some(format!("FineShot ({label})")));
                if let Some(settings) = &mut self.settings {
                    settings.set_status(true, format!("Saved & active: {label}"));
                }
            } else {
                let prev_hk = self.config.parsed_hotkey();
                if tray.hotkey_manager.register(prev_hk).is_ok() {
                    self.active_hotkey = Some(prev_hk);
                }
                let msg = format!(
                    "{} unavailable in OS - try another key",
                    format_hotkey_label(new_hk)
                );
                self.hotkey_error = Some(msg.clone());
                if let Some(settings) = &mut self.settings {
                    settings.set_status(false, msg);
                }
            }
        } else {
            self.config.hotkey = new_hk.to_string();
            let _ = self.config.save();
            if let Some(settings) = &mut self.settings {
                settings.set_status(
                    true,
                    format!("Saved for tray mode: {}", format_hotkey_label(new_hk)),
                );
            }
        }
    }

    fn close_overlay(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(overlay) = &self.overlay {
            overlay.hide_immediately();
        }
        self.overlay = None;
        if self.instant && self.settings.is_none() {
            event_loop.exit();
        } else {
            trim_working_set();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
        if self.instant {
            if let Some(cap) = self.pending_capture.take() {
                self.handle_captured(event_loop, cap);
            } else if self.overlay.is_none() && self.settings.is_none() {
                self.capture_and_open(event_loop);
            }
        } else {
            self.init_tray();
            if self.open_settings_on_start {
                self.open_settings_on_start = false;
                self.open_settings(event_loop);
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::HotKey(hk_ev) => {
                if hk_ev.state == HotKeyState::Pressed
                    && self.active_hotkey.is_some_and(|hk| hk.id() == hk_ev.id)
                {
                    self.capture_and_open(event_loop);
                }
            }
            UserEvent::TrayClick => {
                self.capture_and_open(event_loop);
            }
            UserEvent::MenuAction(id) => {
                let mut action = None;
                if let Some(tray) = &self.tray {
                    if id == tray.take_id {
                        action = Some(0);
                    } else if id == tray.settings_id {
                        action = Some(1);
                    } else if id == tray.updates_id {
                        action = Some(2);
                    } else if id == tray.quit_id {
                        action = Some(3);
                    }
                }
                match action {
                    Some(0) => self.capture_and_open(event_loop),
                    Some(1) => self.open_settings(event_loop),
                    Some(2) => self.trigger_update_check(true),
                    Some(3) => {
                        self.overlay = None;
                        self.settings = None;
                        event_loop.exit();
                    }
                    _ => {}
                }
            }
            UserEvent::SaveFinished(res) => match res {
                Ok(true) => self.close_overlay(event_loop),
                Ok(false) => {
                    if let Some(overlay) = &mut self.overlay {
                        overlay.set_save_dialog_active(false);
                    }
                }
                Err(err) => {
                    eprintln!("Save error: {err}");
                    if let Some(overlay) = &mut self.overlay {
                        overlay.set_save_dialog_active(false);
                    }
                }
            },
            UserEvent::QuickExportDone => {
                trim_working_set();
                if self.instant && self.settings.is_none() {
                    event_loop.exit();
                }
            }
            UserEvent::UpdateAvailable(info) => {
                self.update_status = updater::UpdateStatus::Available(Box::new(info.clone()));
                if let Some(s) = &mut self.settings {
                    s.set_status(true, format!("Update available: {}", info.tag));
                }
                if updater::show_update_available_prompt(&info) {
                    self.update_status = updater::UpdateStatus::Downloading;
                    if let Some(s) = &mut self.settings {
                        s.set_status(true, format!("Downloading {}...", info.tag));
                    }
                    let proxy = self.proxy.clone();
                    let download_url = info.download_url.clone();
                    std::thread::spawn(move || {
                        lower_thread_priority();
                        match updater::download_update(&download_url) {
                            Ok(temp_path) => {
                                let _ = proxy.send_event(UserEvent::UpdateDownloaded(temp_path));
                            }
                            Err(err) => {
                                let _ = proxy.send_event(UserEvent::UpdateFailed(err));
                            }
                        }
                    });
                }
            }
            UserEvent::UpdateNotFound { is_manual } => {
                self.update_status = updater::UpdateStatus::UpToDate;
                if is_manual {
                    if let Some(s) = &mut self.settings {
                        s.set_status(
                            true,
                            format!("Up to date (v{})", updater::CURRENT_VERSION),
                        );
                    }
                    updater::show_up_to_date_dialog();
                }
            }
            UserEvent::UpdateCheckFailed { is_manual, err } => {
                self.update_status = updater::UpdateStatus::Failed(err.clone());
                if is_manual {
                    if let Some(s) = &mut self.settings {
                        s.set_status(false, "Update check failed");
                    }
                    updater::show_check_failed_dialog(&err);
                }
            }
            UserEvent::UpdateDownloaded(temp_path) => {
                if let Some(tray) = self.tray.take() {
                    if let Some(hk) = self.active_hotkey.take() {
                        let _ = tray.hotkey_manager.unregister(hk);
                    }
                    drop(tray);
                }
                self.settings = None;
                if let Some(overlay) = &self.overlay {
                    overlay.hide_immediately();
                }
                self.overlay = None;
                match updater::apply_update(&temp_path) {
                    Ok(()) => event_loop.exit(),
                    Err(err) => {
                        self.update_status = updater::UpdateStatus::Failed(err.clone());
                        updater::show_install_failed_dialog(&err);
                        if !self.instant {
                            self.init_tray();
                        } else {
                            event_loop.exit();
                        }
                    }
                }
            }
            UserEvent::UpdateFailed(err) => {
                self.update_status = updater::UpdateStatus::Failed(err.clone());
                if let Some(s) = &mut self.settings {
                    s.set_status(false, "Update failed");
                }
                updater::show_install_failed_dialog(&err);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(overlay) = &mut self.overlay {
            if let Some(deadline) = overlay.poll_redraw_deadline() {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(settings) = self.settings.as_mut() {
            if settings.window_id() == window_id {
                match settings.handle_event(event, &self.config) {
                    SettingsAction::None => {}
                    SettingsAction::ApplyHotkey(hk) => self.apply_new_hotkey(hk),
                    SettingsAction::SelectFormat(fmt) => {
                        self.config.default_format = fmt;
                        let _ = self.config.save();
                        if let Some(s) = &mut self.settings {
                            s.set_status(
                                true,
                                format!("Default save format: {}", fmt.short_label()),
                            );
                        }
                    }
                    SettingsAction::ToggleAutostart(enable) => match autostart::set_enabled(enable) {
                        Ok(()) => {
                            if let Some(s) = &mut self.settings {
                                s.set_autostart(enable);
                                let msg = if enable {
                                    "Autostart at Windows login: ON"
                                } else {
                                    "Autostart at Windows login: OFF"
                                };
                                s.set_status(true, msg);
                            }
                        }
                        Err(err) => {
                            if let Some(s) = &mut self.settings {
                                s.set_status(false, err);
                            }
                        }
                    },
                    SettingsAction::TakeScreenshot => {
                        self.settings = None;
                        self.capture_and_open(event_loop);
                    }
                    SettingsAction::CheckUpdates => {
                        self.trigger_update_check(true);
                    }
                    SettingsAction::Close => {
                        self.settings = None;
                        if self.instant && self.overlay.is_none() {
                            event_loop.exit();
                        } else {
                            trim_working_set();
                        }
                    }
                }
                return;
            }
        }

        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        if !overlay.owns_window(window_id) {
            return;
        }

        match overlay.handle_window_event(window_id, event) {
            OverlayAction::None => {}
            OverlayAction::CancelAndClose => {
                self.close_overlay(event_loop);
            }
            OverlayAction::OpenSettings => {
                overlay.hide_immediately();
                self.overlay = None;
                trim_working_set();
                self.open_settings(event_loop);
            }
            OverlayAction::CopyAndClose => {
                overlay.hide_immediately();
                let owned = self.overlay.take().unwrap();
                let (capture, sel, items) = owned.into_export_parts();
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    lower_thread_priority();
                    let is_full_single = items.is_empty()
                        && capture.monitors.len() == 1
                        && sel.x == 0
                        && sel.y == 0
                        && sel.w == capture.monitors[0].bounds.w
                        && sel.h == capture.monitors[0].bounds.h;

                    if is_full_single {
                        let mon = &capture.monitors[0];
                        if let Err(err) = export::copy_xrgb_to_clipboard(
                            &mon.pixels_xrgb,
                            mon.bounds.w as u32,
                            mon.bounds.h as u32,
                        ) {
                            eprintln!("Clipboard copy failed: {err}");
                        }
                    } else {
                        let img = tools::composite_region(&capture, sel, &items, None);
                        if let Err(err) = export::copy_to_clipboard(&img) {
                            eprintln!("Clipboard copy failed: {err}");
                        }
                    }
                    drop(capture);
                    drop(items);
                    let _ = proxy.send_event(UserEvent::QuickExportDone);
                });
            }
            OverlayAction::SaveAsync(img) => {
                overlay.set_save_dialog_active(true);
                trim_working_set();
                let proxy = self.proxy.clone();
                let default_format = self.config.default_format;
                std::thread::spawn(move || {
                    lower_thread_priority();
                    let result = match export::pick_save_path_isolated(default_format) {
                        Ok(Some(path)) => export::save_image_to_path(&img, &path, default_format)
                            .map(|()| true),
                        Ok(None) => Ok(false),
                        Err(err) => Err(err),
                    };
                    drop(img);
                    let _ = proxy.send_event(UserEvent::SaveFinished(result));
                });
            }
        }
    }
}

fn spawn_update_check(proxy: EventLoopProxy<UserEvent>, is_manual: bool, delay_ms: u64) {
    std::thread::spawn(move || {
        lower_thread_priority();
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        match updater::check_for_updates() {
            Ok(Some(info)) => {
                let _ = proxy.send_event(UserEvent::UpdateAvailable(info));
            }
            Ok(None) => {
                let _ = proxy.send_event(UserEvent::UpdateNotFound { is_manual });
            }
            Err(err) => {
                let _ = proxy.send_event(UserEvent::UpdateCheckFailed { is_manual, err });
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        trim_working_set();
    });
}

fn build_tray_icon() -> Icon {
    let size = 32u32;
    let mut pix = Pixmap::new(size, size).unwrap();

    let mut bg_paint = Paint::default();
    bg_paint.set_color(Color::from_rgba8(14, 165, 233, 255));
    bg_paint.anti_alias = true;
    let mut pb = PathBuilder::new();
    let r = 6.0;
    let w = 32.0;
    pb.move_to(r, 0.0);
    pb.line_to(w - r, 0.0);
    pb.quad_to(w, 0.0, w, r);
    pb.line_to(w, w - r);
    pb.quad_to(w, w, w - r, w);
    pb.line_to(r, w);
    pb.quad_to(0.0, w, 0.0, w - r);
    pb.line_to(0.0, r);
    pb.quad_to(0.0, 0.0, r, 0.0);
    pb.close();
    if let Some(p) = pb.finish() {
        pix.fill_path(&p, &bg_paint, FillRule::Winding, Transform::identity(), None);
    }

    let mut fg_paint = Paint::default();
    fg_paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    fg_paint.anti_alias = true;
    if let Some(rc) = Rect::from_xywh(7.0, 8.0, 18.0, 16.0) {
        let p = PathBuilder::from_rect(rc);
        let stroke = Stroke {
            width: 2.5,
            ..Stroke::default()
        };
        pix.stroke_path(&p, &fg_paint, &stroke, Transform::identity(), None);
    }
    if let Some(dot) = PathBuilder::from_circle(16.0, 16.0, 3.2) {
        pix.fill_path(&dot, &fg_paint, FillRule::Winding, Transform::identity(), None);
    }

    let mut rgba = pix.take();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        if a > 0 && a < 255 {
            let af = 255.0 / a as f32;
            px[0] = ((px[0] as f32 * af).round() as u32).min(255) as u8;
            px[1] = ((px[1] as f32 * af).round() as u32).min(255) as u8;
            px[2] = ((px[2] as f32 * af).round() as u32).min(255) as u8;
        }
    }
    Icon::from_rgba(rgba, size, size).expect("valid tray icon RGBA")
}

#[cfg(target_os = "windows")]
fn lower_thread_priority() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(h_thread: isize, priority: i32) -> i32;
    }
    const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(not(target_os = "windows"))]
fn lower_thread_priority() {}

#[cfg(target_os = "windows")]
fn trim_working_set() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn SetProcessWorkingSetSize(h_process: isize, min: usize, max: usize) -> i32;
    }
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(not(target_os = "windows"))]
fn trim_working_set() {}

#[cfg(target_os = "windows")]
fn ensure_per_monitor_dpi_aware() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetProcessDpiAwarenessContext(value: isize) -> i32;
    }
    unsafe {
        SetProcessDpiAwarenessContext(-4);
    }
}

#[cfg(not(target_os = "windows"))]
fn ensure_per_monitor_dpi_aware() {}

fn main() {
    ensure_per_monitor_dpi_aware();
    let args: Vec<String> = std::env::args().collect();

    if let Some(pos) = args.iter().position(|a| a == "--save-dialog") {
        let default_name = args
            .get(pos + 1)
            .map(String::as_str)
            .unwrap_or("FineShot.png");
        let default_ext = args.get(pos + 2).map(String::as_str).unwrap_or("png");
        export::run_save_dialog_subprocess(default_name, default_ext);
        return;
    }

    updater::cleanup_old_version();

    let instant = args.iter().any(|a| a == "--instant");
    let open_settings_on_start = args.iter().any(|a| a == "--settings");
    let config = AppConfig::load();

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("Failed to create winit event loop");
    let proxy = event_loop.create_proxy();

    let pending_capture = if instant {
        match capture::capture_virtual_desktop() {
            Ok(cap) => Some(cap),
            Err(err) => {
                eprintln!("Capture error: {err}");
                return;
            }
        }
    } else {
        None
    };

    let mut app = App {
        instant,
        open_settings_on_start,
        config,
        pending_capture,
        overlay: None,
        settings: None,
        tray: None,
        proxy,
        active_hotkey: None,
        hotkey_error: None,
        update_status: updater::UpdateStatus::Idle,
        startup_update_checked: false,
    };

    if let Err(err) = event_loop.run_app(&mut app) {
        eprintln!("Event loop error: {err}");
    }
}
