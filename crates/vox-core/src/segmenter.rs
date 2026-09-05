//! VAD-driven audio segmentation.
//!
//! Audio frames arrive with a per-frame speech/no-speech decision. The segmenter buffers
//! them and cuts a [`Segment`] whenever the speaker pauses long enough (so it can be
//! transcribed while they keep talking), or when a hard maximum is reached. On release,
//! [`Segmenter::finish`] returns the tail as a final segment.
//!
//! Leading silence is bounded to `pre_roll_ms` so a long wait before speaking does not
//! turn into seconds of dead audio in the first segment. Segments with less than
//! `min_speech_ms` of detected speech are dropped — they are clicks and breaths, and
//! Whisper hallucinates on them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SegmenterConfig {
    /// Silence length that ends a segment during speech.
    pub pause_ms: u32,
    /// A segment is not cut on a pause until it is at least this long.
    pub min_segment_ms: u32,
    /// Force a cut at this length even without a pause.
    pub max_segment_ms: u32,
    /// Segments with less detected speech than this are discarded.
    pub min_speech_ms: u32,
    /// Silence kept ahead of the first speech frame.
    pub pre_roll_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        SegmenterConfig {
            pause_ms: 600,
            min_segment_ms: 1500,
            max_segment_ms: 15_000,
            min_speech_ms: 250,
            pre_roll_ms: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Mono samples at the segmenter's sample rate.
    pub samples: Vec<f32>,
    /// `true` for the segment produced by [`Segmenter::finish`].
    pub is_final: bool,
}

#[derive(Debug, Clone)]
pub struct Segmenter {
    cfg: SegmenterConfig,
    sample_rate: u32,
    buf: Vec<f32>,
    speech_ms: u32,
    silence_run_ms: u32,
    has_speech: bool,
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig, sample_rate: u32) -> Self {
        assert!(sample_rate > 0, "sample_rate must be positive");
        Segmenter {
            cfg,
            sample_rate,
            buf: Vec::new(),
            speech_ms: 0,
            silence_run_ms: 0,
            has_speech: false,
        }
    }

    pub fn config(&self) -> &SegmenterConfig {
        &self.cfg
    }

    /// Drops everything buffered.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.speech_ms = 0;
        self.silence_run_ms = 0;
        self.has_speech = false;
    }

    /// Milliseconds of audio currently buffered.
    pub fn buffered_ms(&self) -> u32 {
        self.ms(self.buf.len())
    }

    fn ms(&self, samples: usize) -> u32 {
        (samples as u64 * 1000 / self.sample_rate as u64) as u32
    }

    fn samples(&self, ms: u32) -> usize {
        (ms as u64 * self.sample_rate as u64 / 1000) as usize
    }

    /// Append one frame with its VAD decision. Returns a segment when one is ready.
    pub fn push(&mut self, frame: &[f32], is_speech: bool) -> Option<Segment> {
        if frame.is_empty() {
            return None;
        }
        let frame_ms = self.ms(frame.len());
        self.buf.extend_from_slice(frame);

        if is_speech {
            self.has_speech = true;
            self.speech_ms += frame_ms;
            self.silence_run_ms = 0;
        } else {
            self.silence_run_ms += frame_ms;
        }

        if !self.has_speech {
            // Still waiting for the first word: keep only the pre-roll.
            let keep = self.samples(self.cfg.pre_roll_ms);
            if self.buf.len() > keep {
                let excess = self.buf.len() - keep;
                self.buf.drain(..excess);
            }
            return None;
        }

        let buffered = self.buffered_ms();
        let natural_break =
            self.silence_run_ms >= self.cfg.pause_ms && buffered >= self.cfg.min_segment_ms;
        let forced = buffered >= self.cfg.max_segment_ms;
        if natural_break || forced {
            return self.cut(false);
        }
        None
    }

    /// End of capture: return whatever remains as the final segment, if it contains speech.
    pub fn finish(&mut self) -> Option<Segment> {
        if !self.has_speech {
            self.reset();
            return None;
        }
        self.cut(true)
    }

    fn cut(&mut self, is_final: bool) -> Option<Segment> {
        let enough_speech = self.speech_ms >= self.cfg.min_speech_ms;
        let samples = std::mem::take(&mut self.buf);
        self.speech_ms = 0;
        self.silence_run_ms = 0;
        self.has_speech = false;
        if enough_speech {
            Some(Segment { samples, is_final })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16_000;
    const FRAME_MS: u32 = 30;
    const FRAME: usize = (SR as usize * FRAME_MS as usize) / 1000; // 480 samples

    fn seg() -> Segmenter {
        Segmenter::new(SegmenterConfig::default(), SR)
    }

    /// Feed `ms` of audio, asserting no segment is emitted along the way.
    fn feed_quiet(s: &mut Segmenter, ms: u32, speech: bool) {
        for _ in 0..(ms / FRAME_MS) {
            let f = vec![if speech { 0.5 } else { 0.0 }; FRAME];
            assert!(s.push(&f, speech).is_none());
        }
    }

    /// Feed `ms` of audio, returning the first emitted segment.
    fn feed_until(s: &mut Segmenter, ms: u32, speech: bool) -> Option<Segment> {
        for _ in 0..(ms / FRAME_MS) {
            let f = vec![if speech { 0.5 } else { 0.0 }; FRAME];
            if let Some(seg) = s.push(&f, speech) {
                return Some(seg);
            }
        }
        None
    }

    #[test]
    fn silence_alone_emits_nothing_and_stays_bounded() {
        let mut s = seg();
        feed_quiet(&mut s, 10_000, false);
        assert!(s.buffered_ms() <= s.config().pre_roll_ms);
        assert_eq!(s.finish(), None);
        assert_eq!(s.buffered_ms(), 0);
    }

    #[test]
    fn speech_then_pause_cuts_a_segment_with_pre_roll() {
        let mut s = seg();
        feed_quiet(&mut s, 2_000, false); // long wait before talking
        feed_quiet(&mut s, 1_500, true);
        let out = feed_until(&mut s, 1_000, false).expect("segment after pause");
        assert!(!out.is_final);
        let ms = out.samples.len() as u32 * 1000 / SR;
        // pre-roll (≤300) + 1500 speech + 600 pause ≈ 2400; never the whole 2 s of waiting.
        assert!(ms >= 1500 + 600, "got {ms} ms");
        assert!(ms <= 300 + 1500 + 600 + FRAME_MS, "got {ms} ms");
        assert_eq!(s.buffered_ms(), 0, "buffer reset after cut");
    }

    #[test]
    fn pause_does_not_cut_before_min_segment() {
        let mut s = seg();
        feed_quiet(&mut s, 600, true); // short phrase
        feed_quiet(&mut s, 600, false); // 600 ms pause, but segment only ~1200 ms
        assert!(s.buffered_ms() >= 1200);
        // Keep talking; a later pause cuts once min_segment is satisfied.
        feed_quiet(&mut s, 600, true);
        assert!(feed_until(&mut s, 1_000, false).is_some());
    }

    #[test]
    fn forced_cut_at_max_segment() {
        let mut s = seg();
        let out = feed_until(&mut s, 20_000, true).expect("forced cut");
        let ms = out.samples.len() as u32 * 1000 / SR;
        assert!(
            ms >= s.config().max_segment_ms && ms < s.config().max_segment_ms + FRAME_MS,
            "got {ms} ms"
        );
        assert!(!out.is_final);
    }

    #[test]
    fn short_blip_is_dropped() {
        let mut s = seg();
        feed_quiet(&mut s, 90, true); // 3 frames of "speech" — a click
        feed_quiet(&mut s, 1_500, false);
        assert_eq!(s.finish(), None);
        assert_eq!(s.buffered_ms(), 0);
    }

    #[test]
    fn short_blip_followed_by_pause_resets_without_segment() {
        let mut s = seg();
        feed_quiet(&mut s, 90, true);
        // Enough silence to satisfy pause + min_segment: cut attempted, discarded, and the
        // trailing silence is once again bounded to the pre-roll.
        assert!(feed_until(&mut s, 2_000, false).is_none());
        assert!(s.buffered_ms() <= s.config().pre_roll_ms);
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn finish_returns_final_tail() {
        let mut s = seg();
        feed_quiet(&mut s, 900, true);
        let tail = s.finish().expect("tail");
        assert!(tail.is_final);
        assert!(tail.samples.len() >= 900 * SR as usize / 1000);
        assert_eq!(s.buffered_ms(), 0);
    }

    #[test]
    fn two_utterances_yield_intermediate_then_final() {
        let mut s = seg();
        feed_quiet(&mut s, 2_000, true);
        let first = feed_until(&mut s, 1_000, false).expect("first");
        assert!(!first.is_final);
        feed_quiet(&mut s, 800, true);
        let last = s.finish().expect("last");
        assert!(last.is_final);
    }

    #[test]
    fn empty_frame_is_ignored() {
        let mut s = seg();
        assert_eq!(s.push(&[], true), None);
        assert_eq!(s.buffered_ms(), 0);
    }

    #[test]
    fn reset_drops_buffer() {
        let mut s = seg();
        feed_quiet(&mut s, 600, true);
        s.reset();
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn config_defaults_round_trip_toml() {
        let cfg = SegmenterConfig::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: SegmenterConfig = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg);
        let partial: SegmenterConfig = toml::from_str("pause_ms = 900").unwrap();
        assert_eq!(partial.pause_ms, 900);
        assert_eq!(partial.min_segment_ms, cfg.min_segment_ms);
    }
}
