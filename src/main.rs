#![windows_subsystem = "windows"]

//! Derrick — a GPUI desk sentinel that lives in the system tray.
//!
//! Runs in the background with no window. It only shows up when you click the
//! tray icon, or when a break starts (window pops + alarm). Watches you
//! through the webcam during breaks only; the camera is off during work.
//!
//! Everything runs locally — no frames leave your machine.

mod camera;
mod detect;
mod settings;
mod tracker;
mod tray;
mod updates;

use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, ImageSource, IntoElement, ParentElement, Render,
    RenderImage, Styled, Window, WindowBounds, WindowHandle, WindowOptions, div, img, px, rgb,
    size,
};
use gpui::prelude::*;
use image::{Frame, RgbaImage};

use camera::{CameraEvent, CameraSwitch, spawn_camera};
use detect::{Detector, INPUT_H, INPUT_W};
use tracker::{Classifier, Phase, Presence, Tracker, TrackerEvent};
use tray::{TrayCommand, TrayMsg};
use updates::{UpdateInfo, UpdateStatus, Phase as UpdatePhase};

const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// Project repository — replace with the real URL once the repo exists.
const GITHUB_URL: &str = "https://github.com/SilverGlenn/derrick";



/// How long transient status messages stay visible.
const STATUS_LIFETIME: Duration = Duration::from_secs(6);

// ---------------------------------------------------------------------------
// Application state (lives at App level, independent of any window)
// ---------------------------------------------------------------------------

struct SergeantState {
    tracker: Tracker,
    classifier: Classifier,
    settings: settings::Settings,
    /// Mirror of the autostart registry state (tray menu checkbox + window).
    autostart: bool,
    /// Camera display names, indexed by camera index (empty if query failed).
    cameras: Vec<String>,
    preview: Option<Arc<RenderImage>>,
    camera_switch: CameraSwitch,
    /// Mirror of the switch, so the UI knows the camera is on without poking atomics.
    camera_on: bool,
    camera_ok: bool,
    last_face_score: Option<f32>,
    status: String,
    /// When the transient status message expires (None = not showing).
    status_until: Option<Instant>,
    /// Set when the window should pop to the foreground (break start / tray click).
    should_pop: bool,
    should_quit: bool,
    /// The visible window, if any. `None` = background mode.
    window_handle: Option<WindowHandle<SergeantView>>,
    /// The settings window, if open.
    settings_handle: Option<WindowHandle<SettingsView>>,
    /// Set when the settings window should open.
    should_open_settings: bool,
    /// The About window, if open.
    about_handle: Option<WindowHandle<AboutView>>,
    /// Set when the About window should open.
    should_open_about: bool,
    last_tooltip: String,
    /// Last Clock in/out state sent to the tray (to avoid redundant updates).
    last_clock_state: Option<bool>,
    rx: mpsc::Receiver<CameraEvent>,
    tray_rx: mpsc::Receiver<TrayCommand>,
    tooltip_tx: mpsc::Sender<TrayMsg>,
    _camera_thread: std::thread::JoinHandle<()>,
    _tray_thread: std::thread::JoinHandle<()>,
}

impl SergeantState {
    fn new(cx: &mut Context<Self>) -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut settings = settings::load();
        // The registry is the source of truth for autostart (the config can
        // drift if the user edited the Run key by hand or the exe moved).
        let autostart = settings::autostart_enabled();
        settings.autostart = autostart;
        let camera_index = args
            .iter()
            .find_map(|a| a.strip_prefix("--camera="))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(settings.camera_index);

        let model_path = find_model().expect(
            "face_detection_yunet_2026may.onnx not found — put it in assets/ next to the binary",
        );
        let detector = Detector::new(&model_path)
            .unwrap_or_else(|e| panic!("failed to init detector ({model_path:?}): {e:#}"));

        // Enumerate cameras before the camera thread opens any device.
        let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
            Ok(list) => {
                let names: Vec<String> = list.iter().map(|c| c.human_name()).collect();
                log::debug!("enumerated cameras: {names:?}");
                names
            }
            Err(err) => {
                log::warn!("failed to enumerate cameras: {err}");
                Vec::new()
            }
        };

        let (tx, rx) = mpsc::channel();
        let camera_switch = CameraSwitch::new(); // on at launch: initial calibration
        let camera_thread = spawn_camera(camera_index, detector, camera_switch.clone(), tx);

        let (tray_tx, tray_rx) = mpsc::channel();
        let (tooltip_tx, tooltip_rx) = mpsc::channel();
        let tray_thread = tray::spawn_tray(tray_tx, tooltip_rx, autostart);

        let this = Self {
            tracker: Tracker::new(settings.work_minutes, settings.break_minutes),
            classifier: Classifier::new(),
            settings,
            autostart,
            cameras,
            preview: None,
            camera_switch,
            camera_on: true,
            camera_ok: true,
            last_face_score: None,
            status: String::new(),
            status_until: None,
            should_pop: false,
            should_quit: false,
            window_handle: None,
            settings_handle: None,
            should_open_settings: false,
            about_handle: None,
            should_open_about: false,
            last_tooltip: String::new(),
            last_clock_state: None,
            rx,
            tray_rx,
            tooltip_tx,
            _camera_thread: camera_thread,
            _tray_thread: tray_thread,
        };

        // Main loop: drain camera + tray events, advance the state machine.
        // Lives at App level, so it keeps running with zero windows open.
        let entity = cx.entity();
        let app = cx.to_async();
        let task_app = app.clone();
        app.spawn(async move |_: &mut AsyncApp| {
            let mut app = task_app;
            let mut last = Instant::now();
            let mut tick_count: u64 = 0;
            loop {
                app.background_executor().timer(TICK_INTERVAL).await;
                let now = Instant::now();
                let dt = now.duration_since(last);
                last = now;
                if tick_count % 20 == 0 {
                    log::debug!("tick heartbeat (n={tick_count})");
                }
                tick_count += 1;
                // Window opening/activating must happen OUTSIDE the entity
                // update: opening a window builds the root view, which reads
                // the state entity (observe) — re-entering the entity while
                // it is being updated would panic.
                let (should_pop, should_quit) = entity.update(&mut app, |this, cx| {
                    this.tick(dt, cx);
                    (this.should_pop, this.should_quit)
                });
                if should_quit {
                    log::info!("quitting via tray");
                    app.update(|app| app.quit());
                    return;
                }
                if should_pop {
                    entity.update(&mut app, |this, _| this.should_pop = false);
                    open_or_activate(&entity, &mut app);
                }
                if entity.update(&mut app, |this, _| this.should_open_settings) {
                    entity.update(&mut app, |this, _| this.should_open_settings = false);
                    open_settings(&entity, &mut app);
                }
                if entity.update(&mut app, |this, _| this.should_open_about) {
                    entity.update(&mut app, |this, _| this.should_open_about = false);
                    open_about(&entity, &mut app);
                }
            }
        })
        .detach();

        this
    }

    /// Show a transient status message (cleared after STATUS_LIFETIME).
    fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_until = Some(Instant::now() + STATUS_LIFETIME);
    }

    fn tick(&mut self, dt: Duration, cx: &mut Context<Self>) {
        // Detect the main window being closed (handle.update fails on a dead
        // window). With "close hides to tray" off, closing it quits the app;
        // otherwise we just drop the stale handle.
        if let Some(handle) = &self.window_handle {
            if handle.update(cx, |_, _, _| ()).is_err() {
                log::debug!("main window closed (close_to_tray={})", self.settings.close_to_tray);
                self.window_handle = None;
                if !self.settings.close_to_tray {
                    log::info!("close-to-tray is off — quitting");
                    self.should_quit = true;
                }
            }
        }

        // Expire transient status messages.
        if self.status_until.is_some_and(|t| Instant::now() >= t) {
            self.status.clear();
            self.status_until = None;
        }

        let mut camera_ok = self.camera_ok;
        while let Ok(evt) = self.rx.try_recv() {
            if let Some((bgra, w, h)) = evt.preview {
                if let Some(buffer) = RgbaImage::from_raw(w, h, bgra) {
                    let frame = Frame::new(buffer);
                    let image = RenderImage::new(
                        std::iter::once(frame).collect::<smallvec::SmallVec<[Frame; 1]>>(),
                    );
                    self.preview = Some(Arc::new(image));
                }
            }
            if let Some(faces) = evt.faces {
                self.last_face_score = faces.first().map(|f| f.score);
                self.classifier.classify(&faces);
            }
            if let Some(err) = evt.error {
                if err.is_empty() {
                    camera_ok = true;
                } else {
                    camera_ok = false;
                    self.set_status(format!("Camera problem: {err}"));
                }
            }
        }
        self.camera_ok = camera_ok;

        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                TrayCommand::Show => {
                    self.should_pop = true;
                }
                TrayCommand::Quit => {
                    self.should_quit = true;
                }
                TrayCommand::ClockIn => {
                    self.tracker.clock_in();
                    self.status = String::new(); // the running timer says it all
                }
                TrayCommand::ClockOut => {
                    self.tracker.clock_out();
                    self.set_status("Clocked out. See you later!");
                }
                TrayCommand::ToggleAutostart => {
                    self.toggle_autostart();
                }
            }
        }

        // Camera policy: on for calibration (startup + after each break, so the
        // sitting baseline stays fresh) and during breaks. Off during work.
        let want_camera = self.tracker.phase == Phase::Break || !self.classifier.is_calibrated();
        if want_camera != self.camera_on {
            self.camera_on = want_camera;
            self.camera_switch.set(want_camera);
            self.preview = None;
            self.last_face_score = None;
            log::debug!(
                "gate: phase={:?} calibrated={} -> camera {}",
                self.tracker.phase,
                self.classifier.is_calibrated(),
                if want_camera { "ON" } else { "OFF" }
            );
            // No status text: the eye icon + preview placeholder communicate
            // the camera state.
        }

        // If the camera is down (or off) we cannot verify anything: treat the
        // user as sitting so the break timer does not run on a dead camera.
        let presence = if self.camera_on && self.camera_ok {
            self.classifier.presence
        } else {
            Presence::Sitting
        };

        if let Some(event) = self.tracker.tick(dt, presence) {
            match event {
                TrackerEvent::BreakStarted => {
                    log::debug!("event: BREAK STARTED (presence={presence:?})");
                    self.set_status("BREAK TIME — get up and move!");
                    self.should_pop = true;
                    play_alarm();
                }
                TrackerEvent::BreakCompleted => {
                    log::debug!("event: BREAK COMPLETED (presence={presence:?})");
                    self.set_status("Break complete. Back to the grind!");
                    // Re-learn the sitting baseline while the user settles in.
                    self.classifier.recalibrate();
                }
            }
        }

        // Keep the tray in sync with the clock state (menu item enablement).
        let clocked_in = self.tracker.phase != Phase::Idle;
        if self.last_clock_state != Some(clocked_in) {
            self.last_clock_state = Some(clocked_in);
            let _ = self.tooltip_tx.send(TrayMsg::SetClockState { clocked_in });
        }

        // Keep the tray tooltip in sync (phase + timer).
        let tooltip = if self.tracker.phase == Phase::Idle {
            "Idle — click Clock in to start".to_string()
        } else if self.tracker.phase == Phase::Break {
            let a = self.tracker.break_accumulated;
            let n = self.tracker.break_needed;
            format!(
                "STAND UP! break {:02}:{:02} / {:02}:{:02}",
                a.as_secs() / 60,
                a.as_secs() % 60,
                n.as_secs() / 60,
                n.as_secs() % 60
            )
        } else {
            let t = self.tracker.work_remaining;
            if self.tracker.paused {
                format!("Paused — work {:02}:{:02}", t.as_secs() / 60, t.as_secs() % 60)
            } else {
                format!("Working — {:02}:{:02} to break", t.as_secs() / 60, t.as_secs() % 60)
            }
        };
        if self.last_tooltip != tooltip {
            self.last_tooltip = tooltip.clone();
            let _ = self.tooltip_tx.send(TrayMsg::SetTooltip(tooltip));
        }

        cx.notify();
    }

    fn clock_in(&mut self, _: &mut Context<Self>) {
        self.tracker.clock_in();
        self.status = String::new(); // the running timer says it all
    }

    fn clock_out(&mut self, _: &mut Context<Self>) {
        self.tracker.clock_out();
        self.status = "Clocked out. See you later!".into();
    }

    /// Flip the Windows autostart registry key and sync the tray checkbox.
    fn toggle_autostart(&mut self) {
        let new_state = !self.autostart;
        match settings::set_autostart(new_state) {
            Ok(()) => {
                self.autostart = new_state;
                self.settings.autostart = new_state;
                let _ = settings::save(&self.settings);
                let _ = self.tooltip_tx.send(TrayMsg::SetAutostartChecked(new_state));
                self.set_status(if new_state {
                    "Autostart enabled — I'll be here when Windows boots."
                } else {
                    "Autostart disabled."
                });
            }
            Err(err) => {
                self.set_status(format!("Autostart failed: {err}"));
            }
        }
    }

    /// Apply + persist settings from the settings panel.
    fn save_settings(
        &mut self,
        work: f64,
        break_: f64,
        camera: u32,
        autostart: bool,
        close_to_tray: bool,
    ) {
        self.settings.work_minutes = work;
        self.settings.break_minutes = break_;
        self.settings.camera_index = camera;
        self.settings.autostart = autostart;
        self.settings.close_to_tray = close_to_tray;
        if let Err(err) = settings::save(&self.settings) {
            self.set_status(format!("Could not save settings: {err}"));
            return;
        }
        self.tracker.apply_durations(work, break_);
        if autostart != self.autostart {
            match settings::set_autostart(autostart) {
                Ok(()) => {
                    self.autostart = autostart;
                    let _ = self.tooltip_tx.send(TrayMsg::SetAutostartChecked(autostart));
                }
                Err(err) => {
                    self.set_status(format!("Autostart failed: {err}"));
                }
            }
        }
        self.set_status("Settings saved.");
    }

    fn toggle_pause(&mut self, _: &mut Context<Self>) {
        self.tracker.toggle_pause();
        self.set_status(if self.tracker.paused {
            "Paused — you slack off on your own time now."
        } else {
            "Resumed."
        });
    }

    fn skip_break(&mut self, _: &mut Context<Self>) {
        self.tracker.skip_break();
        self.set_status("Break skipped. Shame on you.");
    }

    fn recalibrate(&mut self, _: &mut Context<Self>) {
        self.classifier.recalibrate();
        self.set_status(if self.tracker.phase == Phase::Break {
            "Recalibrating — SIT DOWN for ~20 seconds to set a new baseline."
        } else {
            "Recalibrating — sit in your chair and face the camera…"
        });
    }

    fn reset_work(&mut self, _: &mut Context<Self>) {
        self.tracker.reset_work();
        self.set_status("Work timer reset.");
    }
}

/// Show the window: activate it if it exists, otherwise create it.
/// Must be called OUTSIDE an entity update (opening a window builds the root
/// view, which reads the state entity — re-entering it would panic).
fn open_or_activate(state: &Entity<SergeantState>, app: &mut AsyncApp) {
    let activated = state.update(app, |s, cx| {
        if let Some(handle) = &s.window_handle {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return true;
            }
            log::debug!("window handle is stale — opening a new window");
            s.window_handle = None;
        }
        false
    });
    if activated {
        log::debug!("activated existing window");
        return;
    }

    let state_entity = state.clone();
    let bounds = app.update(|app| Bounds::centered(None, size(px(420.), px(520.)), app));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None, // frameless: we draw our own drag strip + close button
        is_resizable: false,
        focus: true,
        show: true,
        ..Default::default()
    };
    match app.open_window(options, |_, cx| cx.new(|cx| SergeantView::new(state_entity, cx))) {
        Ok(handle) => {
            log::debug!("opened window");
            state.update(app, |s, _| s.window_handle = Some(handle));
        }
        Err(err) => log::error!("failed to open window: {err:#}"),
    }
}

/// Show the About window: activate it if it exists, otherwise create it.
fn open_about(state: &Entity<SergeantState>, app: &mut AsyncApp) {
    let activated = state.update(app, |s, cx| {
        if let Some(handle) = &s.about_handle {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return true;
            }
            log::debug!("about window handle is stale — opening a new one");
            s.about_handle = None;
        }
        false
    });
    if activated {
        return;
    }

    let bounds = app.update(|app| Bounds::centered(None, size(px(380.), px(480.)), app));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        is_resizable: false,
        focus: true,
        show: true,
        ..Default::default()
    };
    let state_entity = state.clone();
    match app.open_window(options, |_, cx| cx.new(|cx| AboutView::new(state_entity, cx))) {
        Ok(handle) => {
            log::debug!("opened about window");
            state.update(app, |s, _| s.about_handle = Some(handle));
        }
        Err(err) => log::error!("failed to open about window: {err:#}"),
    }
}

/// Open a URL in the default browser (cmd start handles the quoting).
fn open_url(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}

/// Show the settings window: activate it if it exists, otherwise create it.
/// Same constraints as `open_or_activate` — must run outside entity updates.
fn open_settings(state: &Entity<SergeantState>, app: &mut AsyncApp) {
    let activated = state.update(app, |s, cx| {
        if let Some(handle) = &s.settings_handle {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return true;
            }
            log::debug!("settings window handle is stale — opening a new one");
            s.settings_handle = None;
        }
        false
    });
    if activated {
        return;
    }

    let state_entity = state.clone();
    let bounds = app.update(|app| Bounds::centered(None, size(px(420.), px(460.)), app));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        is_resizable: false,
        focus: true,
        show: true,
        ..Default::default()
    };
    match app.open_window(options, |_, cx| cx.new(|cx| SettingsView::new(state_entity, cx))) {
        Ok(handle) => {
            log::debug!("opened settings window");
            state.update(app, |s, _| s.settings_handle = Some(handle));
        }
        Err(err) => log::error!("failed to open settings window: {err:#}"),
    }
}

// ---------------------------------------------------------------------------
// The window's root view — renders the shared state
// ---------------------------------------------------------------------------

struct SergeantView {
    state: Entity<SergeantState>,
    /// Unsaved edits from the settings panel (applied on Save).
    pending: PendingSettings,
    /// Whether the "..." menu is open.
    menu_open: bool,
    /// When set, the Clock out row is armed and needs a second click.
    confirm_clockout_until: Option<Instant>,
}

#[derive(Clone, Copy)]
struct PendingSettings {
    work: f64,
    break_: f64,
    camera: u32,
    autostart: bool,
    close_to_tray: bool,
    /// Whether the camera dropdown is open (settings window only).
    camera_picker_open: bool,
}

impl SergeantView {
    fn new(state: Entity<SergeantState>, cx: &mut Context<Self>) -> Self {
        // Repaint whenever the state changes.
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let pending = {
            let s = state.read(cx);
            PendingSettings {
                work: s.settings.work_minutes,
                break_: s.settings.break_minutes,
                camera: s.settings.camera_index,
                autostart: s.settings.autostart,
                close_to_tray: s.settings.close_to_tray,
                camera_picker_open: false,
            }
        };
        Self {
            state,
            pending,
            menu_open: false,
            confirm_clockout_until: None,
        }
    }
}

impl SettingsHost for SergeantView {
    fn state(&self) -> &Entity<SergeantState> {
        &self.state
    }
    fn pending(&mut self) -> &mut PendingSettings {
        &mut self.pending
    }
}

impl Render for SergeantView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        window.set_window_title(if state.tracker.phase == Phase::Break {
            "STAND UP! — Derrick"
        } else {
            "Derrick"
        });

        let bg = rgb(0x16161e);
        let text = rgb(0xcdd6f4);
        let dim = rgb(0x8a8fa3);

        let (phase_color, timer_text, timer_caption): (gpui::Rgba, String, &str) = match state.tracker.phase {
            Phase::Idle => (
                rgb(0x8a8fa3),
                "--:--".to_string(),
                "Ready when you are.",
            ),
            Phase::Working => {
                let t = state.tracker.work_remaining;
                (
                    rgb(0xA4CE8B),
                    format!("{:02}:{:02}", t.as_secs() / 60, t.as_secs() % 60),
                    "until break",
                )
            }
            Phase::Break => {
                let a = state.tracker.break_accumulated;
                let n = state.tracker.break_needed;
                (
                    rgb(0xF7E49B),
                    format!(
                        "{:02}:{:02} / {:02}:{:02}",
                        a.as_secs() / 60,
                        a.as_secs() % 60,
                        n.as_secs() / 60,
                        n.as_secs() % 60
                    ),
                    "standing up — sit down and it pauses",
                )
            }
        };

        let idle = state.tracker.phase == Phase::Idle;

        // Camera verdict: shown as a small chip overlaid on the preview's
        // bottom-left corner while the camera is live (break / calibrating /
        // error). When the camera is off, the crossed eye + its label are
        // centered inside the preview box instead — so this overlay is None.
        let overlay: Option<(EyeState, &str, gpui::Rgba)> = if !state.camera_on {
            None
        } else if !state.camera_ok {
            Some((EyeState::Error, "Camera error — break paused", rgb(0xBA5A5A)))
        } else if !state.classifier.is_calibrated() {
            Some((EyeState::On, "Calibrating…", rgb(0x86BCBD)))
        } else {
            match state.classifier.presence {
                Presence::Sitting => Some((EyeState::On, "Sitting — break paused", rgb(0xBA5A5A))),
                Presence::Standing => Some((EyeState::On, "Standing", rgb(0xA4CE8B))),
                Presence::Away => Some((EyeState::On, "Away from desk", rgb(0xA4CE8B))),
            }
        };

        let preview = match &state.preview {
            Some(image) => img(ImageSource::Render(image.clone()))
                .w(px(INPUT_W as f32))
                .h(px(INPUT_H as f32))
                .object_fit(gpui::ObjectFit::Contain)
                .rounded_lg()
                .into_any_element(),
            None if state.camera_on => div()
                .w(px(INPUT_W as f32))
                .h(px(INPUT_H as f32))
                .bg(rgb(0x111118))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(dim)
                .child("…")
                .into_any_element(),
            // Camera off: the crossed eye + explanation, centered in the box.
            None => div()
                .w(px(INPUT_W as f32))
                .h(px(INPUT_H as f32))
                .bg(rgb(0x111118))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(img(ImageSource::Render(eye_icon(EyeState::Off))).size(px(30.)))
                .child(
                    div()
                        .text_sm()
                        .text_color(dim)
                        .child("Camera turns on at break time"),
                )
                .into_any_element(),
        };

        // The one big action button: CLOCK IN when idle, PAUSE/Resume otherwise.
        let big_button = if idle {
            big_button("clockin", "CLOCK IN", rgb(0xA4CE8B)).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.state.update(cx, |s, state_cx| s.clock_in(state_cx));
                },
            ))
        } else {
            big_button(
                "pause",
                if state.tracker.paused { "RESUME" } else { "PAUSE" },
                rgb(0x86BCBD),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.update(cx, |s, state_cx| s.toggle_pause(state_cx));
            }))
        };

        // Everything else lives in the "..." menu.
        let menu_open = self.menu_open;
        let phase = state.tracker.phase;
        let confirming_clockout = self
            .confirm_clockout_until
            .is_some_and(|t| Instant::now() < t);
        if !confirming_clockout {
            self.confirm_clockout_until = None;
        }

        div()
            .size_full()
            .bg(bg)
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .p_4() // 16dp container padding (space200)
            .gap_4() // 16dp between distinct sections (space200)
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("title-drag")
                            .flex_1()
                            .window_control_area(gpui::WindowControlArea::Drag)
                            .text_sm()
                            .text_color(dim)
                            .child(current_date_string()),
                    )
                    .child(
                        button("menu", "⋯", false).on_click(cx.listener(|this, _, _, cx| {
                            this.menu_open = !this.menu_open;
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .id("win-close")
                            .window_control_area(gpui::WindowControlArea::Close)
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(dim)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x2a2a3a)).text_color(text))
                            .child("✕"),
                    ),
            )
            // Timer: fixed-height slot. Idle shows the "--:--" placeholder;
            // the slot height is constant so nothing shifts when the loop
            // starts.
            .child(
                div()
                    .h(px(64.)) // space800 — fixed so nothing shifts
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2() // 8dp between timer and caption
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(phase_color)
                            .child(timer_text),
                    )
                    .child(div().text_sm().text_color(dim).child(timer_caption)),
            )
            // Camera preview (fixed size in every state — nothing shifts).
            .child(
                div()
                    .relative()
                    .border_1()
                    .border_color(rgb(0x2a2a3a))
                    .rounded_lg()
                    .child(preview)
                    .when(overlay.is_some(), |el| {
                        let (eye_state, label, color) = overlay.unwrap();
                        // Full-width anchor row so the chip centers like the
                        // rest of the window content.
                        el.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_2()
                                .flex()
                                .justify_center()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2() // 8dp icon-label gap
                                        .bg(rgb(0x16161e))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .child(
                                            img(ImageSource::Render(eye_icon(eye_state)))
                                                .size(px(14.)),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(color)
                                                .child(label),
                                        ),
                                ),
                        )
                    }),
            )
            // The big button: 24dp below the preview (16dp gap + 8dp mt) so
            // the primary action breathes.
            .child(div().mt_2().child(big_button))
            // Status line (ellipsized so long messages can't overflow).
            .child(
                div()
                    .text_xs()
                    .text_color(dim)
                    .text_ellipsis()
                    .child(state.status.clone()),
            )
            // "..." menu: an invisible dismiss layer (closes on any outside
            // click) drawn beneath the panel itself.
            .when(menu_open, |el| {
                el.child(
                    div()
                        .id("menu-dismiss")
                        .absolute()
                        .left_0()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.menu_open = false;
                            this.confirm_clockout_until = None;
                            cx.notify();
                        })),
                )
                .child(menu_panel(idle, phase, confirming_clockout, cx))
            })
    }
}
trait SettingsHost {
    fn state(&self) -> &Entity<SergeantState>;
    fn pending(&mut self) -> &mut PendingSettings;
}

struct SettingsView {
    state: Entity<SergeantState>,
    pending: PendingSettings,
}

impl SettingsView {
    fn new(state: Entity<SergeantState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let pending = {
            let s = state.read(cx);
            PendingSettings {
                work: s.settings.work_minutes,
                break_: s.settings.break_minutes,
                camera: s.settings.camera_index,
                autostart: s.settings.autostart,
                close_to_tray: s.settings.close_to_tray,
                camera_picker_open: false,
            }
        };
        Self { state, pending }
    }
}

impl SettingsHost for SettingsView {
    fn state(&self) -> &Entity<SergeantState> {
        &self.state
    }
    fn pending(&mut self) -> &mut PendingSettings {
        &mut self.pending
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_window_title("Settings — Derrick");
        let bg = rgb(0x16161e);
        let text = rgb(0xcdd6f4);
        let dim = rgb(0x8a8fa3);
        let state = self.state.read(cx);
        let (p_work, p_break, p_camera, p_autostart, p_close_to_tray) = {
            let p = &self.pending;
            (p.work, p.break_, p.camera, p.autostart, p.close_to_tray)
        };
        let cameras = state.cameras.clone();

        div()
            .size_full()
            .bg(bg)
            .relative() // anchor for the camera picker overlay
            .flex()
            .flex_col()
            .p_4() // 16dp container padding
            .gap_4() // 16dp between sections
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("settings-drag")
                            .flex_1()
                            .window_control_area(gpui::WindowControlArea::Drag)
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(text)
                            .child("SETTINGS"),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .window_control_area(gpui::WindowControlArea::Close)
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(dim)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x2a2a3a)).text_color(text))
                            .child("✕"),
                    ),
            )
            .child(settings_panel(
                p_work,
                p_break,
                p_camera,
                p_autostart,
                p_close_to_tray,
                cameras,
                self.pending.camera_picker_open,
                dim,
                cx,
            ))
    }
}

/// The About window: identity, repo link, update checking, attribution.
struct AboutView {
    state: Entity<SergeantState>,
    /// Shared with the update worker threads (check/download run on
    /// plain threads since ureq blocks).
    updates: Arc<UpdateStatus>,
    /// Latest release info once a check succeeded, drives the download button.
    info: Option<UpdateInfo>,
    /// Cached phase so the poll loop only notifies on change.
    cached: Option<UpdatePhase>,
}

impl AboutView {
    fn new(state: Entity<SergeantState>, cx: &mut Context<Self>) -> Self {
        let updates = updates::shared();
        let poll = updates.clone();
        // Poll the shared status so the UI updates while a check or download
        // runs on a worker thread.
        cx.spawn(async move |this, cx| {
            let mut cached: Option<UpdatePhase> = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let phase = poll.phase.lock().unwrap().clone();
                if Some(&phase) != cached.as_ref() {
                    cached = Some(phase.clone());
                    let info = match &phase {
                        UpdatePhase::Checked(Ok(Some(i))) => Some(i.clone()),
                        _ => None,
                    };
                    this.update(cx, |this, cx| {
                        this.info = info;
                        this.cached = cached.clone();
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
        Self {
            state,
            updates,
            info: None,
            cached: None,
        }
    }
}

impl Render for AboutView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_window_title("About Derrick");
        let bg = rgb(0x16161e);
        let text = rgb(0xcdd6f4);
        let dim = rgb(0x8a8fa3);
        let link = rgb(0x86BCBD);
        let sage = rgb(0xA4CE8B);

        // An inline clickable link (GPUI has no link element).
        let inline_link = |id: &'static str, url: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_1()
                .rounded_sm()
                .text_sm()
                .text_color(link)
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x2a2a3a)))
                .child(label)
                .on_click(cx.listener(move |_, _, _, cx| {
                    open_url(url);
                    cx.notify();
                }))
        };

        // Full-width link row for the repo.
        let link_row = |id: &'static str, url: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .w_full()
                .justify_between()
                .items_center()
                .px_3()
                .py_2()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x2a2a3a)))
                .child(div().text_color(text).child(label))
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(url.trim_start_matches("https://")),
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    open_url(url);
                    cx.notify();
                }))
        };

        // A compact action button (sage = primary action, teal = secondary).
        let action = |id: &'static str, label: &'static str, color: gpui::Rgba| {
            div()
                .id(id)
                .px_3()
                .py_1p5()
                .rounded_md()
                .text_sm()
                .text_color(color)
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x23232e)))
                .child(label)
        };

        // A dim status line (no interaction).
        let status_line = |label: String| {
            div()
                .flex()
                .w_full()
                .items_center()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x1d1d26))
                .text_sm()
                .text_color(dim)
                .child(label)
                .into_any_element()
        };

        // Row: leading text + trailing action button.
        let row_with_action = |text_el: gpui::AnyElement, button: gpui::AnyElement| {
            div()
                .flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(text_el)
                .child(button)
                .into_any_element()
        };

        let phase = self.updates.phase.lock().unwrap().clone();

        // The updates section varies with the phase.
        let updates_row: gpui::AnyElement = match &phase {
            UpdatePhase::Idle => {
                let updates = self.updates.clone();
                action("about-check", "Check for updates", link)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let updates = updates.clone();
                        *updates.phase.lock().unwrap() = UpdatePhase::Checking;
                        cx.notify();
                        std::thread::spawn(move || {
                            let result = updates::check_for_update();
                            *updates.phase.lock().unwrap() = UpdatePhase::Checked(result);
                        });
                    }))
                    .into_any_element()
            }
            UpdatePhase::Checking => status_line("Checking for updates…".to_string()),
            UpdatePhase::Checked(Ok(None)) => {
                let updates = self.updates.clone();
                let version = env!("CARGO_PKG_VERSION").to_string();
                row_with_action(
                    div()
                        .text_sm()
                        .text_color(text)
                        .child(format!("You're on the latest version ({version})"))
                        .into_any_element(),
                    action("about-check-again", "Check again", link)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let updates = updates.clone();
                            *updates.phase.lock().unwrap() = UpdatePhase::Checking;
                            cx.notify();
                            std::thread::spawn(move || {
                                let result = updates::check_for_update();
                                *updates.phase.lock().unwrap() = UpdatePhase::Checked(result);
                            });
                        }))
                        .into_any_element(),
                )
            }
            UpdatePhase::Checked(Ok(Some(info))) => {
                let updates = self.updates.clone();
                let info = info.clone();
                let size = if info.size > 0 {
                    format!(" · {:.1} MB", info.size as f64 / 1_000_000.0)
                } else {
                    String::new()
                };
                row_with_action(
                    div()
                        .text_sm()
                        .text_color(text)
                        .child(format!("Version {} is available{}", info.version, size))
                        .into_any_element(),
                    action("about-download", "Download & install", sage)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let updates = updates.clone();
                            let info = info.clone();
                            cx.notify();
                            std::thread::spawn(move || {
                                let _ = updates::download(&info, &updates);
                            });
                        }))
                        .into_any_element(),
                )
            }
            UpdatePhase::Checked(Err(err)) => {
                let updates = self.updates.clone();
                row_with_action(
                    div()
                        .text_sm()
                        .text_color(rgb(0xBA5A5A))
                        .child(format!("Couldn't reach GitHub ({err})"))
                        .into_any_element(),
                    action("about-retry", "Try again", link)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let updates = updates.clone();
                            *updates.phase.lock().unwrap() = UpdatePhase::Checking;
                            cx.notify();
                            std::thread::spawn(move || {
                                let result = updates::check_for_update();
                                *updates.phase.lock().unwrap() = UpdatePhase::Checked(result);
                            });
                        }))
                        .into_any_element(),
                )
            }
            UpdatePhase::Downloading { done, total } => {
                let pct = if *total > 0 {
                    done * 100 / total
                } else {
                    0
                };
                status_line(format!("Downloading… {pct}%"))
            }
            UpdatePhase::Downloaded => {
                let updates = self.updates.clone();
                let state = self.state.clone();
                row_with_action(
                    div().text_sm().text_color(text).child("Update ready").into_any_element(),
                    action("about-install", "Install & restart", sage)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let updates = updates.clone();
                            let msi = updates.download_dest.lock().unwrap().clone();
                            if let Some(msi) = msi {
                                *updates.phase.lock().unwrap() = UpdatePhase::Installing;
                                match updates::launch_updater(&msi) {
                                    Ok(()) => {
                                        state.update(cx, |s, _| s.should_quit = true);
                                    }
                                    Err(e) => {
                                        *updates.phase.lock().unwrap() =
                                            UpdatePhase::Checked(Err(e));
                                    }
                                }
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            }
            UpdatePhase::Installing => status_line("Installing — Derrick will restart".to_string()),
        };

        div()
            .size_full()
            .bg(bg)
            .relative()
            .flex()
            .flex_col()
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("about-drag")
                            .flex_1()
                            .window_control_area(gpui::WindowControlArea::Drag),
                    )
                    .child(
                        div()
                            .id("about-close")
                            .window_control_area(gpui::WindowControlArea::Close)
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_sm()
                            .text_color(dim)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x2a2a3a)).text_color(text))
                            .child("✕"),
                    ),
            )
            // Identity block: icon, name, version + tagline.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(img(ImageSource::Render(app_icon())).size(px(56.)))
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(text)
                            .child("Derrick"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(dim)
                            .child(format!(
                                "Version {} — your desk sentinel",
                                env!("CARGO_PKG_VERSION")
                            )),
                    ),
            )
            // Repo link.
            .child(link_row("about-github", GITHUB_URL, "GitHub repository"))
            // Updates: check, download, install.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(dim)
                            .child("UPDATES"),
                    )
                    .child(updates_row),
            )
            .child(div().w_full().h(px(1.)).bg(rgb(0x2a2a3a)))
            // Attribution as a single flowing line with inline links.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_center()
                            .child(div().text_sm().text_color(dim).child("Break sound: Sound Effect by "))
                            .child(inline_link(
                                "about-pixabay-user",
                                "https://pixabay.com/users/universfield-28281460/",
                                "Universfield",
                            ))
                            .child(div().text_sm().text_color(dim).child(" from "))
                            .child(inline_link(
                                "about-pixabay",
                                "https://pixabay.com/sound-effects/",
                                "Pixabay",
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(dim)
                            .child("used under the Pixabay Content License"),
                    ),
            )
    }
}

/// The app icon (same orange figure as the tray) for the About window.
fn app_icon() -> Arc<RenderImage> {
    use std::sync::OnceLock;
    static ICON: OnceLock<Arc<RenderImage>> = OnceLock::new();
    ICON.get_or_init(|| {
        let png = include_bytes!("../assets/tray_icon.png");
        let img = image::load_from_memory(png)
            .expect("tray icon png")
            .to_rgba8();
        let (w, h) = img.dimensions();
        let buffer = RgbaImage::from_raw(w, h, img.into_raw()).expect("rgba buffer");
        Arc::new(RenderImage::new(
            std::iter::once(Frame::new(buffer)).collect::<smallvec::SmallVec<[Frame; 1]>>(),
        ))
    })
    .clone()
}

/// Actions available from the "..." menu.
#[derive(Clone, Copy)]
enum MenuAction {
    SkipBreak,
    Recalibrate,
    ResetWork,
    ClockOut,
    Settings,
    About,
    Quit,
}

/// One row of the "..." menu.
/// One row of the "..." menu. `danger` rows are red (destructive actions).
fn menu_row(
    id: &'static str,
    label: &'static str,
    disabled: bool,
    danger: bool,
    action: MenuAction,
    cx: &mut Context<SergeantView>,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .py_2() // 8dp — dense menu row (space100)
        .rounded_md()
        .text_sm()
        .text_color(if danger { rgb(0xBA5A5A) } else { rgb(0xcdd6f4) })
        .when(disabled, |el| el.opacity(0.4).cursor_not_allowed())
        .when(!disabled, |el| {
            el.cursor_pointer().hover(|style| {
                style.bg(if danger { rgb(0x2a1f24) } else { rgb(0x2a2a3a) })
            })
        })
        .child(label)
        .when(!disabled, |el| {
            el.on_click(cx.listener(move |this, _, _, cx| {
                // Clock out needs a second, confirming click. Every other row
                // (and this row's first click) resets the confirmation.
                let confirming = this.confirm_clockout_until.is_some();
                this.confirm_clockout_until = None;
                this.menu_open = false;
                match action {
                    MenuAction::SkipBreak => {
                        this.state.update(cx, |s, state_cx| s.skip_break(state_cx))
                    }
                    MenuAction::Recalibrate => {
                        this.state.update(cx, |s, state_cx| s.recalibrate(state_cx))
                    }
                    MenuAction::ResetWork => {
                        this.state.update(cx, |s, state_cx| s.reset_work(state_cx))
                    }
                    MenuAction::ClockOut => {
                        if confirming {
                            this.state.update(cx, |s, state_cx| s.clock_out(state_cx));
                        } else {
                            // First click arms the confirmation; keep the menu open.
                            this.menu_open = true;
                            this.confirm_clockout_until =
                                Some(Instant::now() + Duration::from_secs(4));
                        }
                    }
                    MenuAction::Settings => {
                        this.state.update(cx, |s, _| s.should_open_settings = true)
                    }
                    MenuAction::About => {
                        this.state.update(cx, |s, _| s.should_open_about = true)
                    }
                    MenuAction::Quit => {
                        this.state.update(cx, |s, _| s.should_quit = true)
                    }
                }
                cx.notify();
            }))
        })
}

/// A thin divider between menu sections.
fn menu_separator() -> impl IntoElement {
    div().w_full().h(px(1.)).bg(rgb(0x2a2a3a)).my_1()
}

/// The dropdown panel for the "..." menu. Matches the window background,
/// sections are separated by lines, and Clock out sits in its own danger
/// section with a confirm step.
fn menu_panel(
    idle: bool,
    phase: Phase,
    confirming_clockout: bool,
    cx: &mut Context<SergeantView>,
) -> impl IntoElement {
    let clockout_label = if confirming_clockout {
        "Really clock out?"
    } else {
        "Clock out"
    };
    let mut items: Vec<gpui::AnyElement> = Vec::new();

    if !idle {
        for (id, label, disabled, action) in [
            ("m-skip", "Skip break", phase != Phase::Break, MenuAction::SkipBreak),
            ("m-recal", "Recalibrate", false, MenuAction::Recalibrate),
            ("m-reset", "Reset work", false, MenuAction::ResetWork),
        ] {
            items.push(
                menu_row(id, label, disabled, false, action, cx).into_any_element(),
            );
        }
        items.push(menu_separator().into_any_element());
    }

    items.push(menu_row("m-settings", "Settings", false, false, MenuAction::Settings, cx).into_any_element());
    items.push(menu_separator().into_any_element());
    if !idle {
        // Clock out only exists while clocked in.
        items.push(
            menu_row("m-clockout", clockout_label, false, true, MenuAction::ClockOut, cx)
                .into_any_element(),
        );
    }
    items.push(menu_row("m-about", "About", false, false, MenuAction::About, cx).into_any_element());
    items.push(menu_row("m-quit", "Quit", false, false, MenuAction::Quit, cx).into_any_element());

    div()
        .absolute()
        .right_0()
        .top(px(48.)) // below the top bar — never overlaps the menu button
        .w(px(180.))
        .bg(rgb(0x16161e)) // same as the window
        .border_1()
        .border_color(rgb(0x2a2a3a))
        .rounded_lg()
        .shadow_lg()
        .flex()
        .flex_col()
        .p_2() // 8dp
        .children(items)
}

/// Camera eye icon states.
#[derive(Clone, Copy, PartialEq)]
enum EyeState {
    /// Crossed, dim — camera is off by design.
    Off,
    /// Open — camera is live.
    On,
    /// Crossed, red — camera failed.
    Error,
}

/// A 32x32 eye icon (open or crossed-out), drawn programmatically into an
/// RGBA buffer so there are no font or asset dependencies. Cached per state.
fn eye_icon(state: EyeState) -> Arc<RenderImage> {
    use std::sync::OnceLock;
    static OFF: OnceLock<Arc<RenderImage>> = OnceLock::new();
    static ON: OnceLock<Arc<RenderImage>> = OnceLock::new();
    static ERR: OnceLock<Arc<RenderImage>> = OnceLock::new();
    let cache = match state {
        EyeState::Off => &OFF,
        EyeState::On => &ON,
        EyeState::Error => &ERR,
    };
    cache
        .get_or_init(|| {
            let color: [u8; 3] = match state {
                EyeState::Off => [0x8a, 0x8f, 0xa3],
                EyeState::On => [0x8a, 0x8f, 0xa3],
                EyeState::Error => [0xf7, 0x76, 0x8e],
            };
            let crossed = state != EyeState::On;
            let mut px = vec![0u8; 32 * 32 * 4];
            for y in 0..32 {
                for x in 0..32 {
                    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                    // Almond outline (eye): ellipse ring around (16,16).
                    let dx = (fx - 16.0) / 11.0;
                    let dy = (fy - 16.0) / 6.5;
                    let d = dx * dx + dy * dy;
                    let mut alpha = 0.0f32;
                    if d <= 1.0 && d > 0.68 {
                        alpha = 1.0;
                    }
                    // Pupil.
                    let pdx = fx - 16.0;
                    let pdy = fy - 16.0;
                    if pdx * pdx + pdy * pdy < 3.1 * 3.1 {
                        alpha = 1.0;
                    }
                    // Strike-through line for the crossed state.
                    if crossed {
                        let (x1, y1, x2, y2) = (7.0f32, 7.0f32, 25.0f32, 25.0f32);
                        let seg_len2 = (x2 - x1).powi(2) + (y2 - y1).powi(2);
                        let t = (((fx - x1) * (x2 - x1) + (fy - y1) * (y2 - y1)) / seg_len2)
                            .clamp(0.0, 1.0);
                        let cx = x1 + t * (x2 - x1);
                        let cy = y1 + t * (y2 - y1);
                        let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                        if dist < 1.6 {
                            alpha = 1.0;
                        }
                    }
                    if alpha > 0.0 {
                        let i = (y * 32 + x) * 4;
                        px[i] = color[0];
                        px[i + 1] = color[1];
                        px[i + 2] = color[2];
                        px[i + 3] = 255;
                    }
                }
            }
            let buffer = RgbaImage::from_raw(32, 32, px).expect("32x32 eye buffer");
            let image = RenderImage::new(
                std::iter::once(Frame::new(buffer)).collect::<smallvec::SmallVec<[Frame; 1]>>(),
            );
            Arc::new(image)
        })
        .clone()
}

/// The prominent full-width action button.
fn big_button(
    id: &'static str,
    label: &'static str,
    color: gpui::Rgba,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(40.)) // M3 button height
        .px_10() // 40dp leading/trailing (space500)
        .rounded_lg()
        .flex()
        .items_center()
        .justify_center()
        .text_lg()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(0x16161e))
        .bg(color)
        .cursor_pointer()
        .hover(move |style| style.opacity(0.85))
        .active(move |style| style.opacity(0.7))
        .child(label)
}

fn button(id: &'static str, label: impl Into<gpui::SharedString>, disabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_2() // 8dp
        .rounded_md()
        .text_sm()
        .text_color(rgb(0xcdd6f4))
        .when(disabled, |el| {
            el.bg(rgb(0x2a2a3a)).cursor_not_allowed().opacity(0.5)
        })
        .when(!disabled, |el| {
            el.bg(rgb(0x3a3a4e))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x4a4a5e)))
                .active(|style| style.bg(rgb(0x2a2a3e)))
        })
        .child(label.into())
}

fn small_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_2() // 8dp
        .rounded_md()
        .text_sm()
        .text_color(rgb(0xcdd6f4))
        .bg(rgb(0x3a3a4e))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(0x4a4a5e)))
        .active(|style| style.bg(rgb(0x2a2a3e)))
        .child(label)
}

fn settings_row(
    label: &'static str,
    value: impl IntoElement,
    minus: impl IntoElement,
    plus: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .w_full()
        .justify_between()
        .items_center()
        .child(div().text_sm().text_color(rgb(0x8a8fa3)).child(label))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(minus)
                .child(value)
                .child(plus),
        )
}

/// The settings panel: steppers for work/break/camera, an autostart toggle
/// and a Save button. Values are edited locally and applied on Save.
fn settings_panel<V: SettingsHost + 'static>(
    work: f64,
    break_: f64,
    camera: u32,
    autostart: bool,
    close_to_tray: bool,
    cameras: Vec<String>,
    picker_open: bool,
    dim: gpui::Rgba,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let text = rgb(0xcdd6f4);
    let value_box = |value: String| {
        div()
            .text_sm()
            .text_color(text)
            .w(px(64.))
            .text_center()
            .child(value)
    };

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_4() // 16dp between sections
        // Dismiss layer for the picker: absolute child of the settings root
        // (which is relative). Painted first, so rows/list stay clickable and
        // only background/topbar clicks dismiss the picker.
        .when(picker_open, |el| {
            el.child(
                div()
                    .id("camera-picker-dismiss")
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pending().camera_picker_open = false;
                        cx.notify();
                    })),
            )
        })
        // Section 1: durations + camera (8dp gaps within the group).
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(settings_row(
                    "Work block",
                    value_box(format!("{work:.0} min")),
                    small_button("set-work-minus", "−").on_click(cx.listener(|this, _, _, cx| {
                        this.pending().work = (this.pending().work - 5.0).max(5.0);
                        cx.notify();
                    })),
                    small_button("set-work-plus", "+").on_click(cx.listener(|this, _, _, cx| {
                        this.pending().work = (this.pending().work + 5.0).min(480.0);
                        cx.notify();
                    })),
                ))
                .child(settings_row(
                    "Break",
                    value_box(format!("{break_:.0} min")),
                    small_button("set-break-minus", "−").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.pending().break_ = (this.pending().break_ - 1.0).max(1.0);
                            cx.notify();
                        },
                    )),
                    small_button("set-break-plus", "+").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.pending().break_ = (this.pending().break_ + 1.0).min(120.0);
                            cx.notify();
                        },
                    )),
                ))
        // Camera picker: the options render IN FLOW below the row (GPUI has
        // no z-index, so an overlay list would paint under later siblings).
        .child({
            let display = cameras
                .get(camera as usize)
                .cloned()
                .unwrap_or_else(|| format!("Camera {camera}"));
            let camera_rows: Vec<gpui::AnyElement> = (0..cameras.len().max(1))
                .map(|i| {
                    let label = cameras
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| format!("Camera {i}"));
                    div()
                        .id(format!("cam-{i}"))
                        .px_2()
                        .py_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(if i == camera as usize {
                            rgb(0xA4CE8B)
                        } else {
                            rgb(0xcdd6f4)
                        })
                        .bg(if i == camera as usize {
                            rgb(0x2e3a2a)
                        } else {
                            gpui::Rgba::default()
                        })
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(0x2a2a3a)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pending().camera = i as u32;
                            this.pending().camera_picker_open = false;
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect();
            div()
                .flex()
                .flex_col()
                .w_full()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .child(div().text_sm().text_color(dim).child("Camera"))
                        .child(
                            button("camera-pick", display.clone(), false).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.pending().camera_picker_open =
                                        !this.pending().camera_picker_open;
                                    cx.notify();
                                },
                            )),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child("applies on next start"),
                )
                .when(picker_open, |el| {
                    el.child(
                        div()
                            .w_full()
                            .bg(rgb(0x1a1a26))
                            .border_1()
                            .border_color(rgb(0x2a2a3a))
                            .rounded_lg()
                            .flex()
                            .flex_col()
                            .p_2() // 8dp
                            .children(camera_rows),
                    )
                })
        })
        ) // end of section 1 (durations + camera)

        // Section 2: toggles (8dp gaps within the group).
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .child(div().text_sm().text_color(dim).child("Start with Windows"))
                        .child(
                            button("set-autostart", if autostart { "ON" } else { "OFF" }, false)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pending().autostart = !this.pending().autostart;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(dim)
                                .child("X button hides to tray"),
                        )
                        .child(
                            button(
                                "set-close-to-tray",
                                if close_to_tray { "ON" } else { "OFF" },
                                false,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pending().close_to_tray = !this.pending().close_to_tray;
                                cx.notify();
                            })),
                        ),
                ),
        ) // end of section 2 (toggles)

        // Section 3: primary action — 24dp above (16dp section gap + 8dp).
        .child(
            div()
                .mt_2()
                .flex()
                .w_full()
                .justify_center() // save button: bottom center
                .child(
                    button("set-save", "Save settings", false).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let p = *this.pending();
                            this.state().update(cx, |s, state_cx| {
                                s.save_settings(
                                    p.work,
                                    p.break_,
                                    p.camera,
                                    p.autostart,
                                    p.close_to_tray,
                                );
                                state_cx.notify();
                            });
                        },
                    )),
                ),
        )
}
/// "SATURDAY, AUGUST 1" style date from the system clock.
fn current_date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_date((secs / 86400) as i64)
}

/// Format a days-since-epoch value as "WKD, MON D" (e.g. "SAT, AUG 1").
fn format_date(days: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let weekday = ((days + 4).rem_euclid(7)) as usize;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    format!(
        "{}, {} {}",
        WEEKDAYS[weekday],
        MONTHS[(month - 1) as usize],
        day
    )
}

#[cfg(test)]
mod date_tests {
    #[test]
    fn civil_date_known_values() {
        // Reference days computed against Python's datetime (epoch = Thursday).
        assert_eq!(super::format_date(0), "THU, JAN 1"); // 1970-01-01
        assert_eq!(super::format_date(11016), "TUE, FEB 29"); // 2000-02-29
        assert_eq!(super::format_date(20082), "WED, DEC 25"); // 2024-12-25
        assert_eq!(super::format_date(20666), "SAT, AUG 1"); // 2026-08-01
    }
}

/// Find the ONNX model: next to the exe, next to the exe's parent/assets, or
/// in the CWD. Last resort: the cargo manifest dir (dev builds).
fn find_model() -> Option<PathBuf> {
    const NAME: &str = "face_detection_yunet_2026may.onnx";
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(NAME));
            candidates.push(dir.join("assets").join(NAME));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("assets").join(NAME));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(NAME));
        candidates.push(cwd.join("assets").join(NAME));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets").join(NAME));
    candidates.into_iter().find(|p| p.is_file())
}

/// Play the break sound (fire and forget). The WAV at `assets/bell.wav` is
/// embedded into the executable at build time — replace that file and
/// rebuild to change the sound. Played straight from memory via winmm.
fn play_alarm() {
    use windows_sys::Win32::Media::Audio::{
        PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT,
    };
    static BELL: &[u8] = include_bytes!("../assets/bell.wav");
    let ok = unsafe {
        PlaySoundW(
            BELL.as_ptr() as *const u16,
            std::ptr::null_mut(),
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    log::debug!("break sound played: {ok}");
}

fn main() {
    env_logger::init();

    // Test hooks for the update pipeline (see README).
    if std::env::var_os("SERGEANT_TEST_UPDATES").is_some() {
        std::thread::spawn(|| {
            match updates::fetch_latest() {
                Ok(info) => log::info!("update test: latest release is {}", info.version),
                Err(e) => log::info!("update test: fetch failed: {e}"),
            }
            if std::env::var_os("SERGEANT_TEST_FORCE_UPDATE").is_some() {
                match updates::fetch_latest() {
                    Ok(info) => {
                        log::info!("update test: downloading {}", info.asset_name);
                        let status = updates::shared();
                        match updates::download(&info, &status) {
                            Ok(msi) => match updates::launch_updater(&msi) {
                                Ok(()) => {
                                    log::info!("update test: updater launched, exiting");
                                    std::process::exit(0);
                                }
                                Err(e) => log::error!("update test: updater failed: {e}"),
                            },
                            Err(e) => log::error!("update test: download failed: {e}"),
                        }
                    }
                    Err(e) => log::error!("update test: fetch failed: {e}"),
                }
            }
        });
    }

    // `--selftest <image>`: run the detector on a still image and print results,
    // then exit. Handy for verifying the model + decode pipeline without a camera.
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        let path = arg
            .strip_prefix("--selftest=")
            .map(|s| s.to_string())
            .or_else(|| {
                arg.strip_prefix("--selftest")
                    .map(|_| args.next().unwrap_or_default())
            });
        if let Some(path) = path {
            let path = std::path::PathBuf::from(path);
            let model_path = find_model()
                .expect("model not found — put face_detection_yunet_2026may.onnx in assets/");
            let img = image::open(&path).expect("failed to load image").to_rgb8();
            let mut detector = Detector::new(&model_path)
                .unwrap_or_else(|e| panic!("failed to init detector: {e:#}"));
            match detector.detect(&img) {
                Ok(faces) => {
                    println!("detected {} face(s) in {}:", faces.len(), path.display());
                    for f in &faces {
                        println!(
                            "  score={:.3} box=({:.0},{:.0},{:.0},{:.0})",
                            f.score, f.x, f.y, f.x + f.w, f.y + f.h
                        );
                    }
                }
                Err(e) => {
                    eprintln!("detection failed: {e:#}");
                    std::process::exit(1);
                }
            }
            return;
        }
    }

    // Tray app hardening: never die silently, never run twice.
    install_panic_hook();
    if !ensure_single_instance() {
        std::process::exit(0);
    }
    // Test aid: SERGEANT_TEST_PANIC=1 exercises the crash dialog + crash.log.
    if std::env::var_os("SERGEANT_TEST_PANIC").is_some() {
        panic!("SERGEANT_TEST_PANIC requested");
    }

    gpui_platform::application().run(|cx: &mut App| {
        // The app lives in the tray: closing the window must NOT quit it.
        cx.set_quit_mode(gpui::QuitMode::Explicit);
        // No window at startup: the app lives in the tray and only shows up
        // when the tray icon is clicked or a break starts.
        cx.new(|cx| SergeantState::new(cx));
    });
}

// ---------------------------------------------------------------------------
// Tray-app hardening
// ---------------------------------------------------------------------------

/// Show a native error dialog (background apps have no console to print to).
fn show_message_box(text: &str, caption: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR | MB_OK);
    }
}

/// A panic in a tray app is invisible: the app just vanishes. Log the crash to
/// a file next to the exe and tell the user with a message box.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!("Derrick crashed\n\n{info}\n\n{backtrace}\n");
        let mut path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("crash.log")))
            .unwrap_or_else(std::env::temp_dir);
        if path.is_dir() {
            path = path.join("derrick-crash.log");
        }
        let _ = std::fs::write(&path, &report);
        eprintln!("{report}");
        show_message_box(
            &format!(
                "Derrick crashed.\n\n{info}\n\nCrash details were written to:\n{}",
                path.display()
            ),
            "Derrick crashed",
        );
    }));
}

/// Refuse to run twice: a second instance would fight over the webcam and
/// add a duplicate tray icon. The mutex handle must live for the whole
/// process, so it is leaked deliberately.
fn ensure_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    unsafe {
        let name = windows_sys::core::w!("derrick-singleton");
        let handle = CreateMutexW(std::ptr::null(), 0, name);
        if handle.is_null() {
            // Could not create the mutex (unusual) — allow the run rather than
            // block the user.
            return true;
        }
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
        if already_running {
            show_message_box(
                "Derrick is already running in the system tray.",
                "Already running",
            );
            return false;
        }
        // Note: the handle is a raw pointer and is never closed (no
        // CloseHandle), so the named mutex stays alive for the process
        // lifetime — exactly what we want.
        true
    }
}
