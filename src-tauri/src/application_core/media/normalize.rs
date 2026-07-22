use std::collections::VecDeque;

use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, WindowFunction,
    audioadapter_buffers::direct::SequentialSliceOfVecs,
};

use crate::application_core::{
    domain::{AudioFrame, NORMALIZED_SAMPLE_RATE, VAD_FRAME_SAMPLES},
    error::CoreError,
};

const RESAMPLER_CHUNK_FRAMES: usize = 1_024;

/// Stateful channel downmixing, resampling, and exact 512-sample framing.
pub struct PcmNormalizer {
    input_rate: u32,
    channels: usize,
    input_frames: u64,
    discarded_samples: u64,
    discard_remaining: u64,
    output_samples: u64,
    next_start_sample: u64,
    pending_mono: VecDeque<f32>,
    pending_output: VecDeque<f32>,
    resampler: Option<Async<f32>>,
    input_buffer: Vec<Vec<f32>>,
    output_buffer: Vec<Vec<f32>>,
    delay_to_discard: usize,
    finished: bool,
}

impl PcmNormalizer {
    pub fn new(input_rate: u32, channels: u16, start_sample: u64) -> Result<Self, CoreError> {
        Self::with_discard(input_rate, channels, start_sample, 0)
    }

    /// Build a normalizer that decodes from the beginning but emits at `resume_sample`.
    pub fn for_file_resume(
        input_rate: u32,
        channels: u16,
        resume_sample: u64,
    ) -> Result<Self, CoreError> {
        Self::with_discard(input_rate, channels, resume_sample, resume_sample)
    }

    fn with_discard(
        input_rate: u32,
        channels: u16,
        start_sample: u64,
        discard_output_samples: u64,
    ) -> Result<Self, CoreError> {
        if input_rate == 0 || channels == 0 {
            return Err(CoreError::InvalidArgument(format!(
                "invalid native audio format: {input_rate} Hz, {channels} channels"
            )));
        }
        let mut resampler = if input_rate == NORMALIZED_SAMPLE_RATE {
            None
        } else {
            Some(
                Async::<f32>::new_sinc(
                    f64::from(NORMALIZED_SAMPLE_RATE) / f64::from(input_rate),
                    1.0,
                    &SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2),
                    RESAMPLER_CHUNK_FRAMES,
                    1,
                    FixedAsync::Input,
                )
                .map_err(|error| CoreError::AudioNormalize(error.to_string()))?,
            )
        };
        let output_capacity = resampler
            .as_ref()
            .map_or(RESAMPLER_CHUNK_FRAMES, Resampler::output_frames_max);
        let delay_to_discard = resampler.as_ref().map_or(0, Resampler::output_delay);

        Ok(Self {
            input_rate,
            channels: usize::from(channels),
            input_frames: 0,
            discarded_samples: 0,
            discard_remaining: discard_output_samples,
            output_samples: 0,
            next_start_sample: start_sample,
            pending_mono: VecDeque::with_capacity(RESAMPLER_CHUNK_FRAMES * 2),
            pending_output: VecDeque::with_capacity(output_capacity * 2),
            resampler: resampler.take(),
            input_buffer: vec![vec![0.0; RESAMPLER_CHUNK_FRAMES]],
            output_buffer: vec![vec![0.0; output_capacity]],
            delay_to_discard,
            finished: false,
        })
    }

    pub fn push_interleaved(&mut self, samples: &[f32]) -> Result<Vec<AudioFrame>, CoreError> {
        if self.finished {
            return Err(CoreError::AudioNormalize(
                "audio was pushed after the normalizer was finished".into(),
            ));
        }
        if !samples.len().is_multiple_of(self.channels) {
            return Err(CoreError::AudioNormalize(format!(
                "{} samples do not align to {} channels",
                samples.len(),
                self.channels
            )));
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(CoreError::AudioNormalize(
                "audio contains a non-finite sample".into(),
            ));
        }
        for frame in samples.chunks_exact(self.channels) {
            self.pending_mono
                .push_back(frame.iter().copied().sum::<f32>() / self.channels as f32);
            self.input_frames = self.input_frames.saturating_add(1);
        }
        self.process_complete_chunks()?;
        Ok(self.take_complete_frames(false))
    }

    pub fn finish(&mut self) -> Result<Vec<AudioFrame>, CoreError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        self.process_complete_chunks()?;
        if self.resampler.is_none() {
            self.pending_output.extend(self.pending_mono.drain(..));
        } else if !self.pending_mono.is_empty() {
            let valid = self.pending_mono.len();
            self.input_buffer[0].fill(0.0);
            for destination in self.input_buffer[0].iter_mut().take(valid) {
                *destination = self.pending_mono.pop_front().expect("length checked");
            }
            self.process_resampler(Some(valid))?;
        }

        let expected_samples = self
            .input_frames
            .saturating_mul(u64::from(NORMALIZED_SAMPLE_RATE))
            / u64::from(self.input_rate);
        while self.discarded_samples + self.output_samples + (self.pending_output.len() as u64)
            < expected_samples
            && self.resampler.is_some()
        {
            self.input_buffer[0].fill(0.0);
            self.process_resampler(Some(0))?;
        }
        let remaining = usize::try_from(
            expected_samples.saturating_sub(self.discarded_samples + self.output_samples),
        )
        .map_err(|_| CoreError::AudioNormalize("normalized audio is too large".into()))?;
        self.pending_output.truncate(remaining);
        Ok(self.take_complete_frames(true))
    }

    fn process_complete_chunks(&mut self) -> Result<(), CoreError> {
        if self.resampler.is_none() {
            self.pending_output.extend(self.pending_mono.drain(..));
            return Ok(());
        }
        while self.pending_mono.len() >= RESAMPLER_CHUNK_FRAMES {
            for destination in &mut self.input_buffer[0] {
                *destination = self.pending_mono.pop_front().expect("length checked");
            }
            self.process_resampler(None)?;
        }
        Ok(())
    }

    fn process_resampler(&mut self, partial_len: Option<usize>) -> Result<(), CoreError> {
        let input = SequentialSliceOfVecs::new(&self.input_buffer, 1, RESAMPLER_CHUNK_FRAMES)
            .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
        let output_frames = self
            .resampler
            .as_ref()
            .expect("resampler checked")
            .output_frames_next();
        let mut output = SequentialSliceOfVecs::new_mut(&mut self.output_buffer, 1, output_frames)
            .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;
        let indexing = partial_len.map(|frames| Indexing::new().partial_len(frames));
        let (_, produced) = self
            .resampler
            .as_mut()
            .expect("resampler checked")
            .process_into_buffer(&input, &mut output, indexing.as_ref())
            .map_err(|error| CoreError::AudioNormalize(error.to_string()))?;

        let mut output_start = 0;
        if self.delay_to_discard != 0 {
            let discarded = self.delay_to_discard.min(produced);
            self.delay_to_discard -= discarded;
            output_start = discarded;
        }
        self.pending_output.extend(
            self.output_buffer[0][output_start..produced]
                .iter()
                .copied(),
        );
        Ok(())
    }

    fn take_complete_frames(&mut self, include_partial: bool) -> Vec<AudioFrame> {
        let discarded = self
            .pending_output
            .len()
            .min(usize::try_from(self.discard_remaining).unwrap_or(usize::MAX));
        self.pending_output.drain(..discarded);
        self.discard_remaining -= discarded as u64;
        self.discarded_samples += discarded as u64;

        let mut frames = Vec::new();
        while self.pending_output.len() >= VAD_FRAME_SAMPLES
            || (include_partial && !self.pending_output.is_empty())
        {
            let count = self.pending_output.len().min(VAD_FRAME_SAMPLES);
            let samples: Vec<f32> = self.pending_output.drain(..count).collect();
            let start_sample = self.next_start_sample;
            self.next_start_sample = self.next_start_sample.saturating_add(samples.len() as u64);
            self.output_samples = self.output_samples.saturating_add(samples.len() as u64);
            frames.push(AudioFrame {
                start_sample,
                samples,
            });
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(rate: u32, channels: u16) -> Vec<f32> {
        (0..rate as usize)
            .flat_map(|index| {
                let sample = (index as f32 * 0.01).sin();
                std::iter::repeat_n(sample, usize::from(channels))
            })
            .collect()
    }

    fn normalized(rate: u32, channels: u16, chunk_frames: usize) -> Vec<AudioFrame> {
        let input = input(rate, channels);
        let mut normalizer = PcmNormalizer::new(rate, channels, 37).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(chunk_frames * usize::from(channels)) {
            output.extend(normalizer.push_interleaved(chunk).unwrap());
        }
        output.extend(normalizer.finish().unwrap());
        output
    }

    #[test]
    fn normalizes_common_rates_with_contiguous_sample_boundaries() {
        for (rate, channels) in [(44_100, 1), (48_000, 2), (96_000, 6), (16_000, 2)] {
            let output = normalized(rate, channels, 777);
            assert_eq!(
                output
                    .iter()
                    .map(|frame| frame.samples.len())
                    .sum::<usize>(),
                16_000
            );
            assert!(output.iter().all(|frame| frame.samples.len() <= 512));
            assert_eq!(output.first().unwrap().start_sample, 37);
            for pair in output.windows(2) {
                assert_eq!(
                    pair[1].start_sample,
                    pair[0].start_sample + pair[0].samples.len() as u64
                );
            }
        }
    }

    #[test]
    fn output_is_independent_of_input_chunk_boundaries() {
        let small = normalized(44_100, 2, 113);
        let large = normalized(44_100, 2, 4_096);
        assert_eq!(small, large);
    }

    #[test]
    fn emits_only_one_partial_final_frame_and_finish_is_idempotent() {
        let mut normalizer = PcmNormalizer::new(16_000, 1, 0).unwrap();
        assert_eq!(
            normalizer.push_interleaved(&vec![0.0; 700]).unwrap().len(),
            1
        );
        let final_frames = normalizer.finish().unwrap();
        assert_eq!(final_frames.len(), 1);
        assert_eq!(final_frames[0].samples.len(), 188);
        assert!(normalizer.finish().unwrap().is_empty());
    }

    #[test]
    fn rejects_unaligned_and_non_finite_input() {
        let mut normalizer = PcmNormalizer::new(48_000, 2, 0).unwrap();
        assert!(normalizer.push_interleaved(&[0.0, 1.0, 2.0]).is_err());
        assert!(normalizer.push_interleaved(&[0.0, f32::NAN]).is_err());
    }

    #[test]
    fn file_resume_discards_exact_normalized_samples_before_reframing() {
        let mut normalizer = PcmNormalizer::for_file_resume(16_000, 1, 700).unwrap();
        let samples: Vec<f32> = (0..1_500).map(|value| value as f32).collect();
        let mut output = normalizer.push_interleaved(&samples).unwrap();
        output.extend(normalizer.finish().unwrap());

        assert_eq!(output[0].start_sample, 700);
        assert_eq!(output[0].samples[0], 700.0);
        assert_eq!(output[0].samples.len(), 512);
        assert_eq!(output[1].start_sample, 1_212);
        assert_eq!(output[1].samples.len(), 288);
    }
}
