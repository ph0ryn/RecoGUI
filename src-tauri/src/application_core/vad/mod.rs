mod segmenter;
mod silero;

pub use segmenter::{ProbabilityModel, SpeechSegment, VadConfig, VadSegmenter};
pub use silero::{SILERO_VAD_SHA256, SileroOnnx, validate_silero_asset};

#[cfg(test)]
mod contract_tests {
    use std::collections::VecDeque;

    use serde::Deserialize;

    use super::*;
    use crate::application_core::domain::{
        AudioFrame, NORMALIZED_SAMPLE_RATE, SplitReason, VAD_FRAME_SAMPLES,
    };
    use crate::application_core::error::CoreError;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        sample_rate: u32,
        frame_samples: usize,
        context_samples: usize,
        model_sha256: String,
        zero_frame_probability: f32,
        probability_tolerance: f32,
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        name: String,
        probabilities: Vec<f32>,
        config: Config,
        #[serde(default)]
        flush: bool,
        segments: Vec<ExpectedSegment>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Config {
        min_silence_duration_ms: u32,
        min_speech_duration_ms: u32,
        speech_pad_ms: u32,
        target_segment_duration_ms: u32,
        max_segment_duration_ms: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExpectedSegment {
        start_sample: u64,
        end_sample: u64,
        split_reason: SplitReason,
    }

    struct FixtureModel(VecDeque<f32>);

    impl ProbabilityModel for FixtureModel {
        fn reset(&mut self) {}

        fn probability(&mut self, _: &[f32; VAD_FRAME_SAMPLES]) -> Result<f32, CoreError> {
            Ok(self.0.pop_front().expect("fixture probability exists"))
        }
    }

    #[test]
    fn shared_vad_contract_matches_onnx_and_segmentation_boundaries() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../../../../fixtures/native/vad-cases.json"))
                .unwrap();
        assert_eq!(fixture.sample_rate, NORMALIZED_SAMPLE_RATE);
        assert_eq!(fixture.frame_samples, VAD_FRAME_SAMPLES);
        assert_eq!(fixture.context_samples, 64);
        assert_eq!(fixture.model_sha256, SILERO_VAD_SHA256);
        let asset = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vad/silero_vad.onnx");
        let mut model = SileroOnnx::load(&asset).unwrap();
        let probability = model.probability(&[0.0; VAD_FRAME_SAMPLES]).unwrap();
        assert!(
            (probability - fixture.zero_frame_probability).abs() <= fixture.probability_tolerance
        );

        for case in fixture.cases {
            let config = VadConfig {
                min_silence_duration_ms: case.config.min_silence_duration_ms,
                min_speech_duration_ms: case.config.min_speech_duration_ms,
                speech_pad_ms: case.config.speech_pad_ms,
                target_segment_duration_ms: case.config.target_segment_duration_ms,
                max_segment_duration_ms: case.config.max_segment_duration_ms,
                ..VadConfig::default()
            };
            let count = case.probabilities.len();
            let mut segmenter =
                VadSegmenter::new(FixtureModel(case.probabilities.into()), config).unwrap();
            let mut actual = Vec::new();
            for index in 0..count {
                actual.extend(
                    segmenter
                        .process_frame(AudioFrame {
                            start_sample: (index * VAD_FRAME_SAMPLES) as u64,
                            samples: vec![0.0; VAD_FRAME_SAMPLES],
                        })
                        .unwrap(),
                );
            }
            if case.flush {
                actual.extend(segmenter.flush(true));
            }
            assert_eq!(actual.len(), case.segments.len(), "{}", case.name);
            for (actual, expected) in actual.iter().zip(case.segments) {
                assert_eq!(actual.start_sample, expected.start_sample, "{}", case.name);
                assert_eq!(actual.end_sample(), expected.end_sample, "{}", case.name);
                assert_eq!(actual.split_reason, expected.split_reason, "{}", case.name);
                actual.validate().unwrap();
            }
        }
    }
}
