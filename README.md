# Derrick 🪖

A GPUI desk sentinel that watches you through your webcam and **forces you
to stand up**.

- Every **30 minutes** of work, it demands **5 minutes** of standing or walking.
- The camera checks continuously: if you sit back down at your desk, **the
  break timer pauses** until you're on your feet (or out of frame) again.
- Everything runs locally. No frames ever leave your machine.

## Features

- Live camera preview in a GPUI window
- Face detection via YuNet (ONNX Runtime, DirectML-accelerated GPU inference)
- Auto-calibration: sit in your chair when it starts, and it learns your
  "sitting position" (face position + size). Stand up = face moves up in frame;
  walk away = no face.
- Windows alarm sound when a break starts
- Pause / Resume, Skip break (you cheater), Recalibrate, Reset work timer

## How it behaves now

- **It lives in the system tray.** No window at startup — the app runs in the
  background and only shows up when you **click the tray icon** or when a
  **break starts** (window pops, title changes to `STAND UP!`, alarm plays).
- **The loop only runs while you're clocked in.** The app starts **idle**;
  click **Clock in** (tray menu or window button) to start the work/break
  loop, **Clock out** to stop it and reset the block. The tray menu's
  Clock in/out items enable/disable accordingly.
- **Closing the window hides it back to the tray** — the app keeps running.
  Use the tray menu's **Quit** to actually exit.
- The tray icon's tooltip shows the current phase and timer. Left-click the
  icon to show the window; right-click for the menu.
- **During work the camera is off.** No frames are captured, no light on the
  webcam. It wakes up only when a break starts.
- At launch (and for ~20 s after each break) the camera runs briefly to
  **calibrate your sitting baseline** — sit normally during that.
- The camera stays on for the whole break, checking every 1.5 s: sitting at
  your desk pauses the break timer; standing or leaving the desk lets it run.

## Requirements

- Windows 10/11 with a webcam
- Rust toolchain + MSVC Build Tools (to build)
- Camera privacy setting must allow desktop apps (Windows Settings →
  Privacy → Camera)

## Releases & updates

- **Build the MSI:** `build_msi.cmd` → `dist\Derrick-<version>.msi` (WiX 3.14, downloaded to `tools\` on first run; per-user install, no admin).
- **Publish:** `gh release create v<version> dist\Derrick-<version>.msi --title "Derrick v<version>"`.
- **In-app updates:** the About window's *Check for updates* reads the GitHub Releases API (via curl.exe — see below), downloads the MSI, and silently reinstalls + relaunches.
- The updater keeps `%APPDATA%\Derrick\config.toml` intact.

> Note: HTTP goes through `curl.exe` (bundled with Windows 10 1803+) because some AV products block sockets from unsigned exes like derrick.exe itself.


```sh
cargo build --release
```

or use the wrapper that sets up the MSVC environment:

```sh
./build.sh build --release
```

## Run

```sh
cargo run --release
# or with a specific camera:
derrick.exe --camera=1
```

The ONNX model (`assets/face_detection_yunet_2026may.onnx`) is looked up in
this order: next to the exe, `assets/` next to the exe, `assets/` next to the
exe's parent, the current directory, and the cargo manifest dir.

`DirectML.dll` (ONNX Runtime's GPU backend) must sit next to the exe —
`cargo build` copies it there automatically.

## The window

The window is **frameless** (no OS title bar) and deliberately minimal:

- **Idle:** short date, timer, camera preview and one big **CLOCK IN** button.
  No other text.
- **Clocked in:** the big button becomes **PAUSE** (Resume when paused); the
  camera is a small eye icon — crossed-out while working with
  "Camera turns on at break time", open during breaks with the presence
  verdict (Sitting — break paused / Standing / Away from desk).
- Everything else — Skip break, Recalibrate, Reset work, Clock out,
  Settings, Quit — lives behind the **⋯** menu in the top-right corner.
- **Settings** opens in its own small window (steppers + Save).
- Drag the window by the date strip; the **✕** closes it back to the tray.

## Settings

Work/break lengths and the camera index are saved to
`%APPDATA%\Derrick\config.toml` and can be changed from the
**SETTINGS panel** in the window (steppers + Save). The camera index takes
effect on the next start; durations apply immediately.

**Start with Windows** can be toggled in the settings panel or from the tray
menu ("Start with Windows"). It registers the exe in
`HKCU\...\CurrentVersion\Run`; the registry is the source of truth.

Environment variables (`SERGEANT_WORK_MINUTES`, `SERGEANT_BREAK_MINUTES`,
`--camera=N`) still override the config for testing. Config values are
clamped to sane ranges on load.

## Quick testing

```sh
# Verify the detection pipeline on a still image:
derrick.exe --selftest photo-of-you.jpg

# Fast-forward the work/break cycle (fractional minutes allowed):
SERGEANT_WORK_MINUTES=0.05   # 3-second work block
SERGEANT_BREAK_MINUTES=0.1   # 6-second break

# Skip the Clock in button and start the loop at launch:
SERGEANT_TEST_CLOCKIN=1

# Headless testing aid: skip live calibration, use a synthetic baseline
# ("face centered mid-frame") so the camera gating can be tested without
# anyone in front of the camera:
SERGEANT_TEST_PRESET=1
```

## How the sitting detection works

The webcam should be aimed at your desk/chair. The classifier compares each
face detection against a calibrated baseline:

| Observation                       | Verdict        |
| --------------------------------- | -------------- |
| No face in frame                  | Away           |
| Face much higher than baseline    | Standing       |
| Face much smaller than baseline   | Away           |
| Otherwise                         | Sitting (timer paused) |

Calibration runs automatically over the first ~12 detections (~20 s) — sit
normally during that. Use the **Recalibrate** button if you change your chair,
posture, or camera angle.

If the camera fails mid-break, the timer is treated as **paused** (no free
breaks on a dead camera).

Tuning constants live at the top of `src/detect.rs` (confidence/NMS) and
`src/tracker.rs` (tolerances, durations).

## Architecture

```
src/main.rs    GPUI window, tick loop, camera event draining, buttons
src/camera.rs  nokhwa capture thread -> channel (preview frames + detections)
src/detect.rs  YuNet face detection (ort 2.0 / ONNX Runtime + DirectML)
src/tracker.rs work/break state machine + sitting classifier (calibration)
```

The YuNet preprocessing/decode/NMS is ported 1:1 from the official
implementation (`ShiqiYu/libfacedetection.train`) and validated against
ONNX Runtime in Python (same model, same inputs, matching outputs).

## Known limitations

- Heuristic, not pose estimation: it distinguishes sitting/standing by face
  position/size. A friend sitting in your chair confuses it (that's a feature?
  no. it's a limitation).
- The 30-minute clock runs regardless of whether you're at the desk (only the
  break timer is camera-aware).
- Emoji-free UI because Windows font rendering in GPUI is meh.

## Reliability

- **Single instance:** a second launch shows an "Already running" dialog and
  exits (two instances would fight over the webcam).
- **Crashes are never silent:** a panic writes `crash.log` next to the exe and
  shows a message box instead of vanishing into the background.

## License

MIT. The bundled YuNet model is Apache-2.0 (OpenCV Zoo).
