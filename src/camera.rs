//! Camera capture on a background thread, gated by an enable flag.
//!
//! The camera only streams while `CameraSwitch` is enabled (the UI enables it
//! for calibration and during breaks — never during work). While disabled the
//! thread idles without holding the camera. Frames are letterboxed to the
//! model input size, face detection runs on a fixed cadence, and preview
//! frames (BGRA) + detections are forwarded to the UI thread over an mpsc
//! channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use image::RgbImage;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;

use crate::detect::{Detector, FaceBox, INPUT_H, INPUT_W};

const DETECT_INTERVAL: Duration = Duration::from_millis(1500);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);

/// Shared on/off switch for the camera (UI thread writes, camera thread reads).
#[derive(Clone)]
pub struct CameraSwitch {
    enabled: Arc<AtomicBool>,
}

impl CameraSwitch {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn set(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

impl Default for CameraSwitch {
    fn default() -> Self {
        Self::new()
    }
}

/// One message from the camera thread. Any field may be `None` on a given tick.
pub struct CameraEvent {
    /// BGRA preview frame (INPUT_W x INPUT_H), if a new one is ready.
    pub preview: Option<(Vec<u8>, u32, u32)>,
    /// Fresh face detections, if a detection tick happened.
    pub faces: Option<Vec<FaceBox>>,
    /// Camera error (also used to signal recovery with `Some` -> `None`).
    pub error: Option<String>,
}

/// Spawn the camera thread. The camera is opened only while `switch` is
/// enabled; it re-opens automatically on errors and drops the camera when
/// disabled.
pub fn spawn_camera(
    index: u32,
    mut detector: Detector,
    switch: CameraSwitch,
    tx: mpsc::Sender<CameraEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        // Idle while disabled — no camera handle held.
        while !switch.enabled.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
        log::debug!("camera enabling");

        let mut camera = match Camera::new(
            CameraIndex::Index(index),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution),
        ) {
            Ok(camera) => camera,
            Err(err) => {
                let _ = tx.send(CameraEvent {
                    preview: None,
                    faces: None,
                    error: Some(format!("camera open failed: {err}")),
                });
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        if let Err(err) = camera.open_stream() {
            let _ = tx.send(CameraEvent {
                preview: None,
                faces: None,
                error: Some(format!("camera stream failed: {err}")),
            });
            thread::sleep(Duration::from_secs(3));
            continue;
        }
        let _ = tx.send(CameraEvent {
            preview: None,
            faces: None,
            error: None, // camera is live
        });
        log::debug!("camera stream opened");

        let mut last_detect = Instant::now() - DETECT_INTERVAL;
        let mut last_preview = Instant::now() - PREVIEW_INTERVAL;
        let mut frame_count = 0u64;

        // Stream while enabled.
        while switch.enabled.load(Ordering::Relaxed) {
            let frame = match camera.frame() {
                Ok(frame) => frame,
                Err(err) => {
                    let _ = tx.send(CameraEvent {
                        preview: None,
                        faces: None,
                        error: Some(format!("camera frame failed: {err}")),
                    });
                    break; // drop the camera and retry from the top
                }
            };
            frame_count += 1;
            if frame_count % 50 == 0 {
                log::debug!("captured {frame_count} frames");
            }

            let rgb: RgbImage = match frame.decode_image::<RgbFormat>() {
                Ok(rgb) => rgb,
                Err(err) => {
                    let _ = tx.send(CameraEvent {
                        preview: None,
                        faces: None,
                        error: Some(format!("frame decode failed: {err}")),
                    });
                    continue;
                }
            };
            let small = crate::detect::letterbox(&rgb, INPUT_W, INPUT_H);

            let faces = if last_detect.elapsed() >= DETECT_INTERVAL {
                last_detect = Instant::now();
                match detector.detect(&small) {
                    Ok(faces) => Some(faces),
                    Err(err) => {
                        log::error!("face detection failed: {err:#}");
                        None
                    }
                }
            } else {
                None
            };

            if last_preview.elapsed() >= PREVIEW_INTERVAL {
                last_preview = Instant::now();
                let _ = tx.send(CameraEvent {
                    preview: Some((to_bgra(&small), INPUT_W, INPUT_H)),
                    faces,
                    error: None,
                });
            } else if faces.is_some() {
                let _ = tx.send(CameraEvent {
                    preview: None,
                    faces,
                    error: None,
                });
            }
        }

        let _ = camera.stop_stream();
        log::debug!("camera disabled");
    })
}

/// RGB -> BGRA with opaque alpha (GPUI's RenderImage expects BGRA).
fn to_bgra(rgb: &RgbImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.as_raw().len() / 3 * 4);
    for px in rgb.as_raw().chunks_exact(3) {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    out
}
