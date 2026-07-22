use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::application_core::{
    domain::{AudioFrame, NORMALIZED_SAMPLE_RATE, SplitReason, VAD_FRAME_SAMPLES, VadDiagnostics},
    error::CoreError,
};

pub trait ProbabilityModel {
    fn reset(&mut self);
    fn probability(&mut self, samples: &[f32; VAD_FRAME_SAMPLES]) -> Result<f32, CoreError>;
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VadConfig {
    pub start_threshold: f32,
    pub end_threshold: f32,
    pub min_speech_duration_ms: u32,
    pub min_silence_duration_ms: u32,
    pub speech_pad_ms: u32,
    pub target_segment_duration_ms: u32,
    pub max_segment_duration_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            start_threshold: 0.5,
            end_threshold: 0.35,
            min_speech_duration_ms: 160,
            min_silence_duration_ms: 800,
            speech_pad_ms: 160,
            target_segment_duration_ms: 30_000,
            max_segment_duration_ms: 60_000,
        }
    }
}

impl VadConfig {
    pub fn validate(self) -> Result<Self, CoreError> {
        if !self.start_threshold.is_finite()
            || !self.end_threshold.is_finite()
            || self.end_threshold < 0.0
            || self.end_threshold >= self.start_threshold
            || self.start_threshold > 1.0
        {
            return Err(CoreError::InvalidArgument(
                "VAD thresholds must satisfy 0 <= end < start <= 1".into(),
            ));
        }
        if self.min_speech_duration_ms == 0
            || self.min_silence_duration_ms == 0
            || self.target_segment_duration_ms == 0
            || self.target_segment_duration_ms > self.max_segment_duration_ms
            || self.max_segment_duration_ms > 60_000
            || self.min_speech_duration_ms > self.target_segment_duration_ms
            || self.speech_pad_ms
                > self
                    .min_silence_duration_ms
                    .min(self.max_segment_duration_ms)
        {
            return Err(CoreError::InvalidArgument(
                "VAD duration settings are inconsistent".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpeechSegment {
    pub start_sample: u64,
    pub audio: Vec<f32>,
    pub split_reason: SplitReason,
    pub vad: VadDiagnostics,
}

impl SpeechSegment {
    #[must_use]
    pub fn end_sample(&self) -> u64 {
        self.start_sample + self.audio.len() as u64
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.audio.is_empty() || self.audio.len() > 960_000 {
            return Err(CoreError::InvalidArgument(
                "speech segment must contain 1..=960000 samples".into(),
            ));
        }
        if self.audio.iter().any(|sample| !sample.is_finite()) {
            return Err(CoreError::InvalidArgument(
                "speech segment contains a non-finite sample".into(),
            ));
        }
        self.vad.validate()
    }
}

#[derive(Clone)]
struct BufferedFrame {
    samples: Vec<f32>,
    start_sample: u64,
    speech_probability: f32,
}

impl BufferedFrame {
    fn end_sample(&self) -> u64 {
        self.start_sample + self.samples.len() as u64
    }
}

pub struct VadSegmenter<M> {
    model: M,
    config: VadConfig,
    history: VecDeque<BufferedFrame>,
    active_frames: Vec<BufferedFrame>,
    active: bool,
    active_confirmed: bool,
    has_speech_evidence: bool,
    speech_started_sample: Option<u64>,
    silence_started_sample: Option<u64>,
    last_end_sample: Option<u64>,
    last_frame_was_partial: bool,
}

impl<M: ProbabilityModel> VadSegmenter<M> {
    pub fn new(model: M, config: VadConfig) -> Result<Self, CoreError> {
        Ok(Self {
            model,
            config: config.validate()?,
            history: VecDeque::new(),
            active_frames: Vec::new(),
            active: false,
            active_confirmed: false,
            has_speech_evidence: false,
            speech_started_sample: None,
            silence_started_sample: None,
            last_end_sample: None,
            last_frame_was_partial: false,
        })
    }

    pub fn reset(&mut self) {
        self.model.reset();
        self.history.clear();
        self.active_frames.clear();
        self.active = false;
        self.active_confirmed = false;
        self.has_speech_evidence = false;
        self.speech_started_sample = None;
        self.silence_started_sample = None;
        self.last_end_sample = None;
        self.last_frame_was_partial = false;
    }

    pub fn process_frame(&mut self, frame: AudioFrame) -> Result<Vec<SpeechSegment>, CoreError> {
        if frame.samples.is_empty() || frame.samples.len() > VAD_FRAME_SAMPLES {
            return Err(CoreError::InvalidArgument(format!(
                "VAD frame must contain 1..={VAD_FRAME_SAMPLES} samples"
            )));
        }
        if self.last_frame_was_partial {
            return Err(CoreError::InvalidArgument(
                "a partial VAD frame must be final".into(),
            ));
        }
        if self
            .last_end_sample
            .is_some_and(|expected| expected != frame.start_sample)
        {
            return Err(CoreError::InvalidArgument(format!(
                "VAD audio is not contiguous at sample {}",
                frame.start_sample
            )));
        }
        if frame.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(CoreError::InvalidArgument(
                "VAD frame contains a non-finite sample".into(),
            ));
        }

        self.last_end_sample = Some(frame.start_sample + frame.samples.len() as u64);
        self.last_frame_was_partial = frame.samples.len() < VAD_FRAME_SAMPLES;
        let probability = self.speech_probability(&frame.samples)?;
        let buffered = BufferedFrame {
            samples: frame.samples,
            start_sample: frame.start_sample,
            speech_probability: probability,
        };

        if !self.active {
            self.append_history(buffered.clone());
            if probability >= self.config.start_threshold {
                self.start_segment(buffered.start_sample);
            }
            return Ok(Vec::new());
        }

        let frame_end = buffered.end_sample();
        let frame_midpoint = buffered.start_sample + buffered.samples.len() as u64 / 2;
        self.active_frames.push(buffered);
        if probability >= self.config.end_threshold {
            self.has_speech_evidence = true;
        }
        if probability < self.config.end_threshold {
            self.silence_started_sample
                .get_or_insert(frame.start_sample);
        } else if probability >= self.config.start_threshold {
            self.silence_started_sample = None;
        }

        if self.reached_silence(frame_end) {
            return Ok(self.finish_silence(frame_end));
        }
        let Some(active_start) = self.active_start_sample() else {
            return Ok(Vec::new());
        };
        let target_sample =
            active_start + self.duration_samples(self.config.target_segment_duration_ms);
        if frame_midpoint >= target_sample && probability < self.config.end_threshold {
            let split = frame_midpoint
                .min(active_start + self.duration_samples(self.config.max_segment_duration_ms));
            return Ok(self
                .finalize(split, SplitReason::AdaptiveSplit, true)
                .into_iter()
                .collect());
        }
        if frame_end - active_start >= self.duration_samples(self.config.max_segment_duration_ms) {
            let split = active_start + self.duration_samples(self.config.max_segment_duration_ms);
            return Ok(self
                .finalize(split, SplitReason::AdaptiveSplit, true)
                .into_iter()
                .collect());
        }
        Ok(Vec::new())
    }

    pub fn flush(&mut self, finalize_open_segment: bool) -> Vec<SpeechSegment> {
        let mut segments = Vec::new();
        if finalize_open_segment && !self.active_frames.is_empty() {
            let end_sample = self
                .active_frames
                .last()
                .map_or(0, BufferedFrame::end_sample);
            let speech_started = self.speech_started_sample;
            let speech_end = self.silence_started_sample.unwrap_or(end_sample);
            let has_speech = (self.active_confirmed && self.has_speech_evidence)
                || speech_started.is_some_and(|start| {
                    speech_end.saturating_sub(start)
                        >= self.duration_samples(self.config.min_speech_duration_ms)
                });
            if has_speech {
                let padded_end = self.silence_started_sample.map_or(end_sample, |silence| {
                    end_sample.min(silence + self.duration_samples(self.config.speech_pad_ms))
                });
                if let Some(segment) = self.finalize(padded_end, SplitReason::EndOfInput, false) {
                    segments.push(segment);
                }
            }
        }
        self.reset();
        segments
    }

    fn speech_probability(&mut self, samples: &[f32]) -> Result<f32, CoreError> {
        let mut padded = [0.0_f32; VAD_FRAME_SAMPLES];
        padded[..samples.len()].copy_from_slice(samples);
        Ok(self.model.probability(&padded)?.clamp(0.0, 1.0))
    }

    fn append_history(&mut self, frame: BufferedFrame) {
        let frame_end = frame.end_sample();
        self.history.push_back(frame);
        let history_start = frame_end
            .saturating_sub(self.duration_samples(self.config.speech_pad_ms))
            .saturating_sub(VAD_FRAME_SAMPLES as u64);
        while self
            .history
            .front()
            .is_some_and(|item| item.end_sample() <= history_start)
        {
            self.history.pop_front();
        }
        if self
            .history
            .front()
            .is_some_and(|item| item.start_sample < history_start)
        {
            let mut first = self.history.pop_front().expect("front checked");
            let offset = (history_start - first.start_sample) as usize;
            first.samples.drain(..offset);
            first.start_sample = history_start;
            self.history.push_front(first);
        }
    }

    fn start_segment(&mut self, speech_started_sample: u64) {
        self.active = true;
        self.active_confirmed = false;
        self.has_speech_evidence = true;
        self.active_frames = self.history.drain(..).collect();
        self.speech_started_sample = Some(speech_started_sample);
        self.silence_started_sample = None;
    }

    fn reached_silence(&self, current_end: u64) -> bool {
        self.silence_started_sample.is_some_and(|start| {
            current_end - start >= self.duration_samples(self.config.min_silence_duration_ms)
        })
    }

    fn finish_silence(&mut self, current_end: u64) -> Vec<SpeechSegment> {
        let Some(silence_start) = self.silence_started_sample else {
            return Vec::new();
        };
        let cut = current_end.min(silence_start + self.duration_samples(self.config.speech_pad_ms));
        let speech_started = self.speech_started_sample.unwrap_or(cut);
        let has_speech = (self.active_confirmed && self.has_speech_evidence)
            || silence_start.saturating_sub(speech_started)
                >= self.duration_samples(self.config.min_speech_duration_ms);
        if !has_speech {
            let (_, remaining) = partition_frames(std::mem::take(&mut self.active_frames), cut);
            self.reset_active(remaining);
            return Vec::new();
        }
        self.finalize(cut, SplitReason::Silence, false)
            .into_iter()
            .collect()
    }

    fn finalize(
        &mut self,
        end_sample: u64,
        split_reason: SplitReason,
        keep_active: bool,
    ) -> Option<SpeechSegment> {
        let (left, right) = partition_frames(std::mem::take(&mut self.active_frames), end_sample);
        if left.is_empty() {
            self.reset_active(right);
            return None;
        }
        let start_sample = left[0].start_sample;
        if end_sample <= start_sample {
            self.reset_active(right);
            return None;
        }
        let audio_len = left.iter().map(|frame| frame.samples.len()).sum();
        let mut audio = Vec::with_capacity(audio_len);
        let mut weighted_probability = 0.0_f64;
        let mut peak_probability = 0.0_f32;
        let mut speech_samples = 0_usize;
        for frame in &left {
            audio.extend_from_slice(&frame.samples);
            weighted_probability +=
                f64::from(frame.speech_probability) * frame.samples.len() as f64;
            peak_probability = peak_probability.max(frame.speech_probability);
            if frame.speech_probability >= self.config.end_threshold {
                speech_samples += frame.samples.len();
            }
        }
        if audio.is_empty() {
            self.reset_active(right);
            return None;
        }
        let segment = SpeechSegment {
            start_sample,
            split_reason,
            vad: VadDiagnostics {
                mean_probability: (weighted_probability / audio.len() as f64) as f32,
                peak_probability,
                speech_ratio: speech_samples as f32 / audio.len() as f32,
            },
            audio,
        };
        debug_assert!(segment.validate().is_ok());

        if keep_active {
            self.active = true;
            self.active_confirmed = true;
            self.has_speech_evidence = right
                .iter()
                .any(|frame| frame.speech_probability >= self.config.end_threshold);
            self.silence_started_sample = trailing_silence_start(&right, self.config.end_threshold);
            self.active_frames = right;
            self.speech_started_sample = Some(end_sample);
            self.history.clear();
        } else {
            self.reset_active(right);
        }
        Some(segment)
    }

    fn reset_active(&mut self, remaining: Vec<BufferedFrame>) {
        self.active = false;
        self.active_confirmed = false;
        self.has_speech_evidence = false;
        self.active_frames.clear();
        self.speech_started_sample = None;
        self.silence_started_sample = None;
        self.history.clear();
        for frame in remaining {
            self.append_history(frame);
        }
    }

    fn active_start_sample(&self) -> Option<u64> {
        self.active_frames.first().map(|frame| frame.start_sample)
    }

    fn duration_samples(&self, milliseconds: u32) -> u64 {
        u64::from(milliseconds) * u64::from(NORMALIZED_SAMPLE_RATE) / 1_000
    }
}

fn partition_frames(
    frames: Vec<BufferedFrame>,
    end_sample: u64,
) -> (Vec<BufferedFrame>, Vec<BufferedFrame>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for frame in frames {
        if frame.end_sample() <= end_sample {
            left.push(frame);
        } else if frame.start_sample >= end_sample {
            right.push(frame);
        } else {
            let offset = (end_sample - frame.start_sample) as usize;
            left.push(BufferedFrame {
                samples: frame.samples[..offset].to_vec(),
                start_sample: frame.start_sample,
                speech_probability: frame.speech_probability,
            });
            right.push(BufferedFrame {
                samples: frame.samples[offset..].to_vec(),
                start_sample: end_sample,
                speech_probability: frame.speech_probability,
            });
        }
    }
    (left, right)
}

fn trailing_silence_start(frames: &[BufferedFrame], threshold: f32) -> Option<u64> {
    let mut silence_start = None;
    for frame in frames {
        if frame.speech_probability < threshold {
            silence_start.get_or_insert(frame.start_sample);
        } else {
            silence_start = None;
        }
    }
    silence_start
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakeModel {
        probabilities: VecDeque<f32>,
        resets: usize,
    }

    impl FakeModel {
        fn new(probabilities: impl IntoIterator<Item = f32>) -> Self {
            Self {
                probabilities: probabilities.into_iter().collect(),
                resets: 0,
            }
        }
    }

    impl ProbabilityModel for FakeModel {
        fn reset(&mut self) {
            self.resets += 1;
        }

        fn probability(&mut self, _: &[f32; VAD_FRAME_SAMPLES]) -> Result<f32, CoreError> {
            Ok(self.probabilities.pop_front().unwrap())
        }
    }

    fn frame(index: u64, size: usize) -> AudioFrame {
        AudioFrame {
            start_sample: index * VAD_FRAME_SAMPLES as u64,
            samples: vec![index as f32 + 1.0; size],
        }
    }

    fn test_config() -> VadConfig {
        VadConfig {
            min_silence_duration_ms: 64,
            min_speech_duration_ms: 32,
            speech_pad_ms: 0,
            target_segment_duration_ms: 1_000,
            max_segment_duration_ms: 2_000,
            ..VadConfig::default()
        }
    }

    #[test]
    fn silence_boundary_is_sample_accurate() {
        let mut engine = VadSegmenter::new(
            FakeModel::new([0.0, 0.0, 0.9, 0.9, 0.0, 0.0]),
            VadConfig {
                speech_pad_ms: 64,
                ..test_config()
            },
        )
        .unwrap();
        let mut segments = Vec::new();
        for index in 0..6 {
            segments.extend(engine.process_frame(frame(index, 512)).unwrap());
        }
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_sample, 0);
        assert_eq!(segments[0].end_sample(), 6 * 512);
        assert_eq!(segments[0].split_reason, SplitReason::Silence);
    }

    #[test]
    fn adaptive_split_carries_every_sample_without_overlap() {
        let mut engine = VadSegmenter::new(
            FakeModel::new([0.9; 5]),
            VadConfig {
                target_segment_duration_ms: 64,
                max_segment_duration_ms: 96,
                ..test_config()
            },
        )
        .unwrap();
        let mut segments = Vec::new();
        for index in 0..5 {
            segments.extend(engine.process_frame(frame(index, 512)).unwrap());
        }
        segments.extend(engine.flush(true));
        assert_eq!(
            segments
                .iter()
                .map(|segment| (segment.start_sample, segment.end_sample()))
                .collect::<Vec<_>>(),
            [(0, 3 * 512), (3 * 512, 5 * 512)]
        );
    }

    #[test]
    fn low_probability_boundary_respects_a_non_aligned_hard_limit() {
        let mut engine = VadSegmenter::new(
            FakeModel::new([0.9, 0.0]),
            VadConfig {
                min_speech_duration_ms: 1,
                target_segment_duration_ms: 45,
                max_segment_duration_ms: 45,
                ..test_config()
            },
        )
        .unwrap();
        assert!(engine.process_frame(frame(0, 512)).unwrap().is_empty());
        let segments = engine.process_frame(frame(1, 512)).unwrap();
        assert_eq!(segments[0].end_sample(), 720);
    }

    #[test]
    fn partial_final_frame_is_not_padded_in_the_emitted_audio() {
        let mut engine = VadSegmenter::new(
            FakeModel::new([0.9]),
            VadConfig {
                min_speech_duration_ms: 1,
                ..test_config()
            },
        )
        .unwrap();
        engine.process_frame(frame(0, 16)).unwrap();
        let segments = engine.flush(true);
        assert_eq!(segments[0].audio.len(), 16);
        assert_eq!(segments[0].end_sample(), 16);
    }

    #[test]
    fn rejects_sequence_gaps_and_audio_after_a_partial_frame() {
        let mut gap = VadSegmenter::new(FakeModel::new([0.0, 0.0]), test_config()).unwrap();
        gap.process_frame(frame(0, 512)).unwrap();
        assert!(gap.process_frame(frame(2, 512)).is_err());

        let mut partial = VadSegmenter::new(FakeModel::new([0.0, 0.0]), test_config()).unwrap();
        partial.process_frame(frame(0, 10)).unwrap();
        assert!(partial.process_frame(frame(1, 512)).is_err());
    }
}
