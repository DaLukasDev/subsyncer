//! Silero VAD (v6, ONNX) speech detection on 16 kHz mono audio.
//!
//! The model takes 512-sample windows (32 ms) prefixed with the last 64
//! samples of the previous window, plus a recurrent state tensor carried
//! between calls. Probabilities are thresholded with hysteresis and
//! resampled onto the 10 ms timeline grid.

use anyhow::{Context, Result};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::TensorRef;
use subsyncer_core::timeline::{FRAME_MS, Timeline};

/// Silero VAD v6.2.3 (MIT), from github.com/snakers4/silero-vad.
static MODEL: &[u8] = include_bytes!("../assets/silero_vad.onnx");

/// Bump whenever the model or post-processing changes; part of the cache key.
pub const VAD_VERSION: &str = "silero-6.2.3/h50-35/g100-s100";

pub const SAMPLE_RATE: usize = 16_000;
pub const WINDOW: usize = 512;
const CONTEXT: usize = 64;
const WINDOW_MS: f64 = WINDOW as f64 * 1000.0 / SAMPLE_RATE as f64; // 32 ms

const THRESHOLD: f32 = 0.5;
const NEG_THRESHOLD: f32 = 0.35;
/// Gaps shorter than this inside speech are filled.
const MIN_GAP_MS: i64 = 100;
/// Speech runs shorter than this are dropped.
const MIN_SPEECH_MS: i64 = 100;

pub struct Vad {
    session: Session,
    state: Vec<f32>,
    input: Vec<f32>,
}

impl Vad {
    pub fn new() -> Result<Self> {
        let session = Session::builder(environment()?)
            .map_err(ort_err)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ort_err)?
            .with_intra_threads(1)
            .map_err(ort_err)?
            .with_inter_threads(1)
            .map_err(ort_err)?
            .commit_from_memory(MODEL)
            .map_err(ort_err)
            .context("loading Silero VAD model")?;
        Ok(Self { session, state: vec![0.0; 2 * 128], input: vec![0.0; CONTEXT + WINDOW] })
    }

    pub fn reset(&mut self) {
        self.state.fill(0.0);
        self.input.fill(0.0);
    }

    /// Speech probability of one 512-sample window (samples in [-1, 1]).
    pub fn process(&mut self, window: &[f32]) -> Result<f32> {
        debug_assert_eq!(window.len(), WINDOW);
        // input = [context (last 64 samples of previous window), window]
        self.input.copy_within(WINDOW.., 0);
        self.input[CONTEXT..].copy_from_slice(window);
        let sr = [SAMPLE_RATE as i64];
        let outputs = self
            .session
            .run(ort::inputs![
                "input" => TensorRef::from_array_view(([1usize, CONTEXT + WINDOW], &self.input[..])).map_err(ort_err)?,
                "state" => TensorRef::from_array_view(([2usize, 1, 128], &self.state[..])).map_err(ort_err)?,
                "sr" => TensorRef::from_array_view(((), &sr[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)?;
        let (_, prob) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        let p = prob.first().copied().unwrap_or(0.0);
        let (_, state) = outputs[1].try_extract_tensor::<f32>().map_err(ort_err)?;
        self.state.copy_from_slice(state);
        Ok(p)
    }
}

/// ONNX Runtime allows one environment per process.
fn environment() -> Result<&'static ort::environment::Environment> {
    static ENV: std::sync::OnceLock<std::result::Result<ort::environment::Environment, String>> =
        std::sync::OnceLock::new();
    ENV.get_or_init(|| ort::init().with_name("subsyncer").with_telemetry(false).build().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("initialising onnxruntime: {e}"))
}

fn ort_err<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("onnxruntime: {e}")
}

/// Streaming speech detector: feed s16 samples, get a timeline at the end.
pub struct SpeechDetector {
    vad: Vad,
    pending: Vec<f32>,
    probs: Vec<f32>,
}

impl SpeechDetector {
    pub fn new() -> Result<Self> {
        Ok(Self { vad: Vad::new()?, pending: Vec::with_capacity(WINDOW * 2), probs: Vec::new() })
    }

    pub fn push_s16(&mut self, samples: &[i16]) -> Result<()> {
        for &s in samples {
            self.pending.push(s as f32 / 32768.0);
            if self.pending.len() == WINDOW {
                let p = self.vad.process(&self.pending)?;
                self.probs.push(p);
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// Number of samples consumed so far.
    pub fn samples(&self) -> usize {
        self.probs.len() * WINDOW + self.pending.len()
    }

    pub fn finish(mut self) -> Result<(Timeline, Vec<f32>)> {
        let total_samples = self.samples();
        if !self.pending.is_empty() {
            let mut w = std::mem::take(&mut self.pending);
            w.resize(WINDOW, 0.0);
            let p = self.vad.process(&w)?;
            self.probs.push(p);
        }
        let n_frames = (total_samples as i64 * 1000 / SAMPLE_RATE as i64 / FRAME_MS) as usize;
        Ok((probs_to_timeline(&self.probs, n_frames), self.probs))
    }
}

/// Hysteresis thresholding, gap filling and resampling to the 10 ms grid.
pub fn probs_to_timeline(probs: &[f32], n_frames: usize) -> Timeline {
    let mut speech = Vec::with_capacity(probs.len());
    let mut on = false;
    for &p in probs {
        on = if on { p >= NEG_THRESHOLD } else { p >= THRESHOLD };
        speech.push(on);
    }
    let mut frames = vec![false; n_frames];
    for (f, x) in frames.iter_mut().enumerate() {
        let center_ms = f as f64 * FRAME_MS as f64 + FRAME_MS as f64 / 2.0;
        let w = (center_ms / WINDOW_MS) as usize;
        *x = speech.get(w).copied().unwrap_or(false);
    }
    smooth(&mut frames, (MIN_GAP_MS / FRAME_MS) as usize, (MIN_SPEECH_MS / FRAME_MS) as usize);
    Timeline::from_frames(frames)
}

fn smooth(frames: &mut [bool], min_gap: usize, min_speech: usize) {
    // Fill short gaps between speech runs.
    let runs = |frames: &[bool], val: bool| -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < frames.len() {
            if frames[i] == val {
                let s = i;
                while i < frames.len() && frames[i] == val {
                    i += 1;
                }
                out.push((s, i));
            } else {
                i += 1;
            }
        }
        out
    };
    for (s, e) in runs(frames, false) {
        if s > 0 && e < frames.len() && e - s < min_gap {
            frames[s..e].fill(true);
        }
    }
    for (s, e) in runs(frames, true) {
        if e - s < min_speech {
            frames[s..e].fill(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_loads_and_silence_is_not_speech() {
        let mut det = SpeechDetector::new().unwrap();
        det.push_s16(&vec![0i16; SAMPLE_RATE * 2]).unwrap();
        let (tl, probs) = det.finish().unwrap();
        assert_eq!(tl.len(), 200);
        assert!(probs.iter().all(|&p| p < 0.2), "{probs:?}");
        assert_eq!(tl.active_fraction(), 0.0);
    }

    #[test]
    fn smoothing() {
        let mut f = vec![true, true, false, true, true, false, false, false, true, false];
        smooth(&mut f, 2, 2);
        assert_eq!(f, vec![true, true, true, true, true, false, false, false, false, false]);
    }
}
