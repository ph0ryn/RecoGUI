mod decode;
mod fingerprint;
mod normalize;

pub use decode::{
    DecodedAudioChunk, FileDecoder, NormalizedFileDecoder, SUPPORTED_AUDIO_EXTENSIONS,
};
pub use fingerprint::{FileFingerprint, FileIdentity, fingerprint_file};
pub use normalize::PcmNormalizer;

#[cfg(test)]
mod contract_tests {
    use std::io::Write;

    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        output: Output,
        supported_extensions: Vec<String>,
        unsupported_extensions: Vec<String>,
        fingerprint: Fingerprint,
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Output {
        sample_rate: u32,
        channels: u16,
        frame_samples: usize,
        sample_type: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fingerprint {
        utf8_contents: String,
        value: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        name: String,
        input: Input,
        expected: Option<Vec<f32>>,
        expected_frame_lengths: Option<Vec<usize>>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Input {
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    }

    #[test]
    fn shared_audio_contract_matches_normalization_extensions_and_fingerprint() {
        let fixture: Fixture = serde_json::from_str(include_str!(
            "../../../../fixtures/native/audio-normalize-cases.json"
        ))
        .unwrap();
        assert_eq!(fixture.output.sample_rate, 16_000);
        assert_eq!(fixture.output.channels, 1);
        assert_eq!(fixture.output.frame_samples, 512);
        assert_eq!(fixture.output.sample_type, "f32");
        assert_eq!(
            fixture.supported_extensions,
            SUPPORTED_AUDIO_EXTENSIONS
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert!(
            fixture
                .unsupported_extensions
                .iter()
                .all(|extension| { !SUPPORTED_AUDIO_EXTENSIONS.contains(&extension.as_str()) })
        );

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(fixture.fingerprint.utf8_contents.as_bytes())
            .unwrap();
        file.flush().unwrap();
        assert_eq!(
            fingerprint_file(file.path()).unwrap().value,
            fixture.fingerprint.value
        );

        for case in fixture.cases {
            let mut normalizer =
                PcmNormalizer::new(case.input.sample_rate, case.input.channels, 0).unwrap();
            let mut frames = normalizer.push_interleaved(&case.input.samples).unwrap();
            frames.extend(normalizer.finish().unwrap());
            if let Some(expected) = case.expected {
                let actual = frames
                    .iter()
                    .flat_map(|frame| frame.samples.iter().copied())
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "{}", case.name);
            }
            if let Some(expected) = case.expected_frame_lengths {
                assert_eq!(
                    frames
                        .iter()
                        .map(|frame| frame.samples.len())
                        .collect::<Vec<_>>(),
                    expected,
                    "{}",
                    case.name
                );
            }
        }
    }
}
