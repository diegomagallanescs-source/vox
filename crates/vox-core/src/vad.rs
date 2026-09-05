//! Voice-activity detection.
//!
//! The production VAD is Silero, provided by the whisper engine crate. [`EnergyVad`] here is
//! a deliberately simple RMS-threshold detector with hangover: a fallback when the engine has
//! no VAD, and a deterministic stand-in for tests.

use crate::traits::Vad;

#[derive(Debug, Clone)]
pub struct EnergyVad {
    threshold_rms: f32,
    hangover_frames: u32,
    remaining: u32,
}

impl EnergyVad {
    /// `threshold_rms` in linear amplitude (samples are −1.0..1.0). `hangover_frames` keeps
    /// reporting speech for that many frames after the level drops, bridging short gaps
    /// between words.
    pub fn new(threshold_rms: f32, hangover_frames: u32) -> Self {
        EnergyVad {
            threshold_rms,
            hangover_frames,
            remaining: 0,
        }
    }

    pub fn rms(frame: &[f32]) -> f32 {
        if frame.is_empty() {
            return 0.0;
        }
        let sum: f32 = frame.iter().map(|s| s * s).sum();
        (sum / frame.len() as f32).sqrt()
    }
}

impl Default for EnergyVad {
    fn default() -> Self {
        // −40 dBFS, ~150 ms hangover at 30 ms frames.
        EnergyVad::new(0.01, 5)
    }
}

impl Vad for EnergyVad {
    fn is_speech(&mut self, frame: &[f32]) -> bool {
        if Self::rms(frame) >= self.threshold_rms {
            self.remaining = self.hangover_frames;
            true
        } else if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }

    fn reset(&mut self) {
        self.remaining = 0;
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_basics() {
        assert_eq!(EnergyVad::rms(&[]), 0.0);
        assert_eq!(EnergyVad::rms(&[0.0; 10]), 0.0);
        assert!((EnergyVad::rms(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn threshold_and_hangover() {
        let mut v = EnergyVad::new(0.1, 2);
        let loud = [0.5f32; 16];
        let quiet = [0.0f32; 16];
        assert!(!v.is_speech(&quiet));
        assert!(v.is_speech(&loud));
        assert!(v.is_speech(&quiet), "hangover 1");
        assert!(v.is_speech(&quiet), "hangover 2");
        assert!(!v.is_speech(&quiet), "hangover exhausted");
    }

    #[test]
    fn reset_clears_hangover() {
        let mut v = EnergyVad::new(0.1, 5);
        v.is_speech(&[0.5f32; 16]);
        v.reset();
        assert!(!v.is_speech(&[0.0f32; 16]));
    }
}
