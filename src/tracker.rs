//! The productivity state machine and the sitting/standing classifier.

use std::time::Duration;

use crate::detect::{FaceBox, INPUT_H, INPUT_W};

pub const DEFAULT_WORK_MINUTES: f64 = 30.0;
pub const DEFAULT_BREAK_MINUTES: f64 = 5.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Presence {
    Sitting,
    Standing,
    Away,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Not clocked in: no timers running, camera off.
    Idle,
    Working,
    Break,
}

pub enum TrackerEvent {
    BreakStarted,
    BreakCompleted,
}

/// Work/break timer. During a break, the countdown only advances while the
/// user is NOT sitting at their desk.
pub struct Tracker {
    pub phase: Phase,
    pub work_remaining: Duration,
    pub break_accumulated: Duration,
    pub break_needed: Duration,
    /// Configured work block length (used when re-arming after a break).
    work_duration: Duration,
    pub paused: bool,
}

impl Tracker {
    /// Build a tracker; durations come from settings, overridable by env vars
    /// (testing aid). Starts in the Idle phase — the loop only begins once the
    /// user clocks in (unless SERGEANT_TEST_CLOCKIN=1).
    pub fn new(settings_work_minutes: f64, settings_break_minutes: f64) -> Self {
        let work_remaining = env_minutes("SERGEANT_WORK_MINUTES", settings_work_minutes);
        let break_needed = env_minutes("SERGEANT_BREAK_MINUTES", settings_break_minutes);
        // Testing aid: start the loop immediately instead of waiting for the
        // Clock in button.
        let auto_clockin = std::env::var("SERGEANT_TEST_CLOCKIN").is_ok_and(|v| v == "1");
        Self {
            phase: if auto_clockin { Phase::Working } else { Phase::Idle },
            work_remaining,
            work_duration: work_remaining,
            break_accumulated: Duration::ZERO,
            break_needed,
            paused: false,
        }
    }

    /// Apply new durations from settings. If a block is in progress it restarts
    /// with the new duration (predictable, no mid-block surprises).
    pub fn apply_durations(&mut self, work_minutes: f64, break_minutes: f64) {
        self.work_duration = Duration::from_secs_f64(work_minutes.max(0.0) * 60.0);
        self.break_needed = Duration::from_secs_f64(break_minutes.max(0.0) * 60.0);
        if self.phase != Phase::Idle {
            self.work_remaining = self.work_duration;
            self.break_accumulated = Duration::ZERO;
            self.phase = Phase::Working;
        }
    }

    /// Start the work/break loop (from Idle).
    pub fn clock_in(&mut self) {
        if self.phase == Phase::Idle {
            self.phase = Phase::Working;
            self.work_remaining = self.work_duration;
            self.break_accumulated = Duration::ZERO;
            self.paused = false;
        }
    }

    /// Stop the loop and go back to Idle (resets the block).
    pub fn clock_out(&mut self) {
        if self.phase != Phase::Idle {
            self.phase = Phase::Idle;
            self.work_remaining = self.work_duration;
            self.break_accumulated = Duration::ZERO;
            self.paused = false;
        }
    }

    /// Advance the state machine by `dt` given the current presence.
    pub fn tick(&mut self, dt: Duration, presence: Presence) -> Option<TrackerEvent> {
        if self.paused {
            return None;
        }
        match self.phase {
            Phase::Idle => {}
            Phase::Working => {
                self.work_remaining = self.work_remaining.saturating_sub(dt);
                if self.work_remaining.is_zero() {
                    self.phase = Phase::Break;
                    self.break_accumulated = Duration::ZERO;
                    return Some(TrackerEvent::BreakStarted);
                }
            }
            Phase::Break => {
                // Sitting at the desk -> break timer is PAUSED.
                if presence != Presence::Sitting {
                    self.break_accumulated = (self.break_accumulated + dt).min(self.break_needed);
                    if self.break_accumulated >= self.break_needed {
                        self.phase = Phase::Working;
                        self.work_remaining = self.work_duration;
                        return Some(TrackerEvent::BreakCompleted);
                    }
                }
            }
        }
        None
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
    }

    /// Skip the current break (cheat code).
    pub fn skip_break(&mut self) {
        if self.phase == Phase::Break {
            self.phase = Phase::Working;
            self.work_remaining = self.work_duration;
        }
    }

    pub fn reset_work(&mut self) {
        self.phase = Phase::Working;
        self.work_remaining = self.work_duration;
        self.break_accumulated = Duration::ZERO;
    }
}

/// Classifies detections into [Presence].
///
/// Heuristics (validated against a camera pointed at the desk/chair):
///   - no face in frame            -> Away
///   - face much higher than the   -> Standing
///     calibrated sitting position
///   - face much smaller than the  -> Away (person moved far back)
///     calibrated sitting size
///   - otherwise                   -> Sitting
///
/// Calibration: the first `CALIBRATION_SAMPLES` detections build a baseline
/// (EMA). During calibration everything is treated as Sitting. Call
/// `recalibrate()` to start over.
pub struct Classifier {
    /// Baseline (normalized cx, cy, face height). None until calibrated.
    baseline: Option<(f32, f32, f32)>,
    samples: u32,
    /// True only after CALIBRATION_SAMPLES face samples were accumulated.
    /// (baseline is populated from the first sample, so it alone cannot
    /// signal "done" — that was a bug that ended calibration after 1 sample.)
    calibrated: bool,
    pub presence: Presence,
    pending: Option<Presence>,
}

const CALIBRATION_SAMPLES: u32 = 12;
const Y_TOLERANCE: f32 = 0.14; // normalized frame height
const MIN_SIZE_RATIO: f32 = 0.45;

impl Classifier {
    pub fn new() -> Self {
        let test_preset = std::env::var("SERGEANT_TEST_PRESET").is_ok_and(|v| v == "1");
        if test_preset {
            // Testing aid: skip live calibration, seed a "face centered mid-frame"
            // baseline so the camera-gating logic can be exercised headlessly.
            return Self {
                baseline: Some((0.5, 0.5, 0.35)),
                samples: CALIBRATION_SAMPLES,
                calibrated: true,
                presence: Presence::Sitting,
                pending: None,
            };
        }
        Self {
            baseline: None,
            samples: 0,
            calibrated: false,
            presence: Presence::Sitting,
            pending: None,
        }
    }

    pub fn is_calibrated(&self) -> bool {
        self.calibrated
    }

    pub fn recalibrate(&mut self) {
        self.baseline = None;
        self.samples = 0;
        self.calibrated = false;
        self.presence = Presence::Sitting;
        self.pending = None;
    }

    /// Feed a detection result. Normalized coordinates use INPUT_W/INPUT_H.
    pub fn classify(&mut self, faces: &[FaceBox]) {
        let next = match faces.first() {
            None => Presence::Away,
            Some(face) => {
                let cx = face.center_x() / INPUT_W as f32;
                let cy = face.center_y() / INPUT_H as f32;
                let fh = face.h / INPUT_H as f32;

                if self.samples < CALIBRATION_SAMPLES {
                    // Calibrating: EMA toward the running mean of the face
                    // position/size. Everything counts as Sitting for now.
                    let (bx, by, bh) = match self.baseline {
                        None => (cx, cy, fh),
                        Some((bx, by, bh)) => (
                            bx * 0.9 + cx * 0.1,
                            by * 0.9 + cy * 0.1,
                            bh * 0.9 + fh * 0.1,
                        ),
                    };
                    self.samples += 1;
                    if self.samples >= CALIBRATION_SAMPLES {
                        self.calibrated = true;
                    }
                    self.baseline = Some((bx, by, bh));
                    Presence::Sitting
                } else {
                    let (_, by, bh) = self.baseline.unwrap();
                    let size_ratio = fh / bh;
                    if size_ratio < MIN_SIZE_RATIO {
                        Presence::Away // too small: not you at the desk
                    } else if cy < by - Y_TOLERANCE {
                        Presence::Standing // face is notably higher up
                    } else {
                        Presence::Sitting
                    }
                }
            }
        };

        // Debounce: a state only flips after two consecutive samples agree.
        if next == self.presence {
            self.pending = None;
        } else if self.pending == Some(next) {
            self.presence = next;
            self.pending = None;
        } else {
            self.pending = Some(next);
        }
    }
}

/// Read a duration override from the environment (fractional minutes allowed,
/// e.g. `SERGEANT_WORK_MINUTES=0.05` for a 3-second work block).
fn env_minutes(name: &str, default: f64) -> Duration {
    let minutes = std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default);
    Duration::from_secs_f64(minutes.max(0.0) * 60.0)
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new(DEFAULT_WORK_MINUTES, DEFAULT_BREAK_MINUTES)
    }
}

impl Default for Classifier {
    fn default() -> Self {
        Self::new()
    }
}
