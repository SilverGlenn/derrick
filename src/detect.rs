//! YuNet face detection via ONNX Runtime (ort 2.0).
//!
//! Model: `face_detection_yunet_2026may.onnx` (OpenCV Zoo, Apache-2.0).
//!
//! Preprocessing, anchor decode and NMS are ported from the official YuNet
//! implementation (ShiqiYu/libfacedetection.train):
//!   - inference pipeline: `yunet_train/cli/compare_inference.py`
//!   - anchor decode:      `yunet_train/tasks/face/codec.py`
//!   - priors:             `yunet_train/engine/priors.py`
//!   - postprocess:        `yunet_train/tasks/face/postprocess.py`
//!
//! Input convention (verified against the reference implementation):
//!   - HWC image resized/letterboxed to `INPUT_W` x `INPUT_H` (both divisible by 32)
//!   - BGR channel order, float32, values in 0..255 (no mean/std normalization)
//!   - output tensors are raw anchor predictions; score = cls * obj (logits, no sigmoid)

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use image::{Rgb, RgbImage};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::{DynValue, Tensor};

/// Model input size. Both dimensions must be divisible by 32 (the ONNX graph
/// has hard-coded feature-map arithmetics that break otherwise).
pub const INPUT_W: u32 = 320;
pub const INPUT_H: u32 = 256;

const CONF_THRESHOLD: f32 = 0.6;
const NMS_THRESHOLD: f32 = 0.3;
const STRIDES: [usize; 3] = [8, 16, 32];

/// A detected face, in `INPUT_W` x `INPUT_H` pixel coordinates.
#[derive(Clone, Copy, Debug)]
pub struct FaceBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
}

impl FaceBox {
    pub fn center_x(&self) -> f32 {
        self.x + self.w * 0.5
    }
    pub fn center_y(&self) -> f32 {
        self.y + self.h * 0.5
    }
}

pub struct Detector {
    session: Session,
    input_name: String,
}

impl Detector {
    pub fn new(model_path: &Path) -> Result<Self> {
        let session = Session::builder()
            .map_err(|e| anyhow!("failed to create ONNX Runtime session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::All)
            // Level-3 optimizations are unavailable in minimal ORT builds; the
            // error is recoverable (the session still works), so ignore it.
            .unwrap_or_else(|e| e.recover())
            .with_intra_threads(2)
            .map_err(|e| anyhow!("failed to set intra-op thread count: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| anyhow!("failed to load model '{}': {e}", model_path.display()))?;

        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| anyhow!("model has no inputs"))?;

        Ok(Self { session, input_name })
    }

    /// Detect faces in a frame of arbitrary size (it is letterboxed to the
    /// model input size internally). Returns boxes in `INPUT_W` x `INPUT_H` space.
    pub fn detect(&mut self, frame: &RgbImage) -> Result<Vec<FaceBox>> {
        let scaled = letterbox(frame, INPUT_W, INPUT_H);
        let (w, h) = (INPUT_W as usize, INPUT_H as usize);

        // Build the NCHW float32 blob in BGR channel order (the convention the
        // model was exported against). Values stay in 0..255, no normalization.
        let mut data = Vec::with_capacity(3 * w * h);
        let raw = scaled.as_raw();
        for ch in 0..3 {
            let src_ch = 2 - ch; // BGR: channel 0 <- R(2), channel 1 <- G(1), channel 2 <- B(0)
            for y in 0..h {
                for x in 0..w {
                    data.push(raw[(y * w + x) * 3 + src_ch] as f32);
                }
            }
        }
        let tensor = Tensor::from_array(([1usize, 3usize, h, w], data))
            .map_err(|e| anyhow!("failed to build input tensor: {e}"))?;

        let outputs = self
            .session
            .run(vec![(self.input_name.clone(), tensor)])
            .map_err(|e| anyhow!("inference failed: {e}"))?;

        let get_out = |name: &str| -> Result<&DynValue> {
            outputs
                .get(name)
                .ok_or_else(|| anyhow!("model output '{name}' missing"))
        };

        // --- Decode raw anchor predictions (official YuNet postprocess) ---
        let mut candidates: Vec<FaceBox> = Vec::new();
        for &stride in STRIDES.iter() {
            let (_, cls) = get_out(&format!("cls_{stride}"))?
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow!("extract cls_{stride}: {e}"))?;
            let (_, obj) = get_out(&format!("obj_{stride}"))?
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow!("extract obj_{stride}: {e}"))?;
            let (_, reg) = get_out(&format!("bbox_{stride}"))?
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow!("extract bbox_{stride}: {e}"))?;

            // Tensors are [1, N, 1] / [1, N, 4] so the flat slices are N / 4N.
            let n = cls.len();
            let feat_h = (INPUT_H as usize) / stride;
            let feat_w = (INPUT_W as usize) / stride;
            if n != feat_h * feat_w {
                bail!(
                    "unexpected anchor count for stride {stride}: {n} (expected {})",
                    feat_h * feat_w
                );
            }

            let s = stride as f32;
            for iy in 0..feat_h {
                for ix in 0..feat_w {
                    let i = iy * feat_w + ix;
                    // score = cls * obj on raw logits (reference implementation
                    // does NOT apply sigmoid before the product)
                    let score = cls[i] * obj[i];
                    if score < CONF_THRESHOLD {
                        continue;
                    }
                    let ax = ix as f32 * s;
                    let ay = iy as f32 * s;
                    let cx = reg[i * 4] * s + ax;
                    let cy = reg[i * 4 + 1] * s + ay;
                    let bw = reg[i * 4 + 2].exp() * s;
                    let bh = reg[i * 4 + 3].exp() * s;
                    candidates.push(FaceBox {
                        x: cx - bw * 0.5,
                        y: cy - bh * 0.5,
                        w: bw,
                        h: bh,
                        score,
                    });
                }
            }
        }

        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // --- Greedy NMS (threshold 0.3, same as FaceDetectorYN default) ---
        let mut kept: Vec<FaceBox> = Vec::new();
        for cand in candidates {
            if kept.iter().all(|k| iou(k, &cand) < NMS_THRESHOLD) {
                kept.push(cand);
            }
        }
        Ok(kept)
    }
}

fn iou(a: &FaceBox, b: &FaceBox) -> f32 {
    let ix = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0.0);
    let iy = ((a.y + a.h).min(b.y + b.h) - a.y.max(b.y)).max(0.0);
    let inter = ix * iy;
    let area_a = a.w * a.h;
    let area_b = b.w * b.h;
    inter / (area_a + area_b - inter)
}

/// Resize an image into `tw` x `th` preserving aspect ratio with black bars.
pub fn letterbox(rgb: &RgbImage, tw: u32, th: u32) -> RgbImage {
    let (w, h) = (rgb.width(), rgb.height());
    if (w, h) == (tw, th) {
        return rgb.clone();
    }
    let scale = (tw as f32 / w as f32).min(th as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(rgb, nw, nh, image::imageops::FilterType::Triangle);
    let mut canvas = RgbImage::from_pixel(tw, th, Rgb([0, 0, 0]));
    let ox = (tw - nw) / 2;
    let oy = (th - nh) / 2;
    image::imageops::overlay(&mut canvas, &resized, ox as i64, oy as i64);
    canvas
}
