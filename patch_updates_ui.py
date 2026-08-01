# Rewrite AboutView: update checking UI + worker threads; wire open_about
import io

p = r"C:\Users\Plas-IT-Radg\derrick\src\main.rs"
s = io.open(p, encoding="utf-8").read()

# 1. module + imports
s = s.replace('''mod tracker;
mod tray;''', '''mod tracker;
mod tray;
mod updates;''')
s = s.replace('''use camera::{CameraEvent, CameraSwitch, spawn_camera};
use detect::{Detector, INPUT_H, INPUT_W};
use tracker::{Classifier, Phase, Presence, Tracker, TrackerEvent};
use tray::{TrayCommand, TrayMsg};''', '''use camera::{CameraEvent, CameraSwitch, spawn_camera};
use detect::{Detector, INPUT_H, INPUT_W};
use tracker::{Classifier, Phase, Presence, Tracker, TrackerEvent};
use tray::{TrayCommand, TrayMsg};
use updates::{UpdateInfo, UpdateStatus, Phase as UpdatePhase};''')

# 2. open_about: pass state entity, bigger window
s = s.replace('''    match app.open_window(options, |_, cx| cx.new(|cx| AboutView::new(cx))) {''',
'''    let state_entity = state.clone();
    match app.open_window(options, |_, cx| cx.new(|cx| AboutView::new(state_entity, cx))) {''')
s = s.replace('''    let bounds = app.update(|app| Bounds::centered(None, size(px(380.), px(260.)), app));''',
'''    let bounds = app.update(|app| Bounds::centered(None, size(px(380.), px(480.)), app));''')

# 3. Replace the AboutView struct + impl + render + app_icon
start = s.index("/// The About window: name, version, repo link, sound attribution.")
end = s.index("/// Actions available from the \"...\" menu.")
new_about = '''/// The About window: identity, repo link, update checking, attribution.
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
        cx.spawn(|this, mut cx| async move {
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
                    this.update(&mut cx, |this, cx| {
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

'''
s = s[:start] + new_about + s[end:]

io.open(p, "w", encoding="utf-8", newline="\n").write(s)
print("about view rewritten")
