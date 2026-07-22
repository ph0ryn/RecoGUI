use std::{
    collections::VecDeque,
    fs::File,
    path::{Path, PathBuf},
};

use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions},
    errors::Error as SymphoniaError,
    formats::{FormatOptions, FormatReader, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

use crate::application_core::{
    domain::AudioFrame,
    error::CoreError,
    media::{FileIdentity, PcmNormalizer},
};

pub const SUPPORTED_AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "caf", "flac", "m4a", "mp3", "ogg", "wav",
];

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudioChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Streaming decoder for the intentionally supported built-in Symphonia codecs.
pub struct FileDecoder {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    identity_handle: File,
    opened_identity: FileIdentity,
    finished: bool,
}

impl FileDecoder {
    pub fn open(path: &Path, expected_identity: Option<&FileIdentity>) -> Result<Self, CoreError> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| CoreError::UnsupportedAudio("file has no extension".into()))?;
        if !SUPPORTED_AUDIO_EXTENSIONS.contains(&extension.as_str()) {
            return Err(CoreError::UnsupportedAudio(format!(
                "unsupported extension: .{extension}"
            )));
        }

        let source = File::open(path)?;
        let identity_handle = source.try_clone()?;
        let opened_identity = FileIdentity::from_file(&identity_handle)?;
        if expected_identity.is_some_and(|expected| expected != &opened_identity) {
            return Err(CoreError::FileChanged);
        }

        let stream = MediaSourceStream::new(Box::new(source), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(&extension);
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|error| CoreError::UnsupportedAudio(error.to_string()))?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| CoreError::UnsupportedAudio("container has no audio track".into()))?;
        let parameters = track
            .codec_params
            .as_ref()
            .and_then(|parameters| parameters.audio())
            .ok_or_else(|| {
                CoreError::UnsupportedAudio("audio codec parameters are missing".into())
            })?;
        let sample_rate = parameters
            .sample_rate
            .ok_or_else(|| CoreError::UnsupportedAudio("audio sample rate is missing".into()))?;
        let channels = parameters
            .channels
            .as_ref()
            .map(|channels| channels.count())
            .ok_or_else(|| CoreError::UnsupportedAudio("audio channel layout is missing".into()))?;
        let channels = u16::try_from(channels)
            .map_err(|_| CoreError::UnsupportedAudio("audio has too many channels".into()))?;
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(parameters, &AudioDecoderOptions::default())
            .map_err(|error| CoreError::UnsupportedAudio(error.to_string()))?;

        Ok(Self {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            identity_handle,
            opened_identity,
            finished: false,
        })
    }

    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }

    pub fn decode_next(&mut self) -> Result<Option<DecodedAudioChunk>, CoreError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let Some(packet) = self.format.next_packet().map_err(map_decode_error)? else {
                self.finished = true;
                self.verify_identity()?;
                return Ok(None);
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = self.decoder.decode(&packet).map_err(map_decode_error)?;
            let decoded_rate = decoded.spec().rate();
            let decoded_channels = u16::try_from(decoded.spec().channels().count())
                .map_err(|_| CoreError::AudioDecode("decoded channel count is too large".into()))?;
            if decoded_rate != self.sample_rate || decoded_channels != self.channels {
                return Err(CoreError::AudioDecode(format!(
                    "audio format changed within one stream: expected {} Hz/{} channels, got {decoded_rate} Hz/{decoded_channels} channels",
                    self.sample_rate, self.channels
                )));
            }
            let mut samples = vec![0.0_f32; decoded.samples_interleaved()];
            decoded.copy_to_slice_interleaved(&mut samples);
            if samples.is_empty() {
                continue;
            }
            if samples.iter().any(|sample| !sample.is_finite()) {
                return Err(CoreError::AudioDecode(
                    "decoder returned a non-finite sample".into(),
                ));
            }
            return Ok(Some(DecodedAudioChunk {
                samples,
                sample_rate: self.sample_rate,
                channels: self.channels,
            }));
        }
    }

    fn verify_identity(&self) -> Result<(), CoreError> {
        if FileIdentity::from_file(&self.identity_handle)? == self.opened_identity {
            Ok(())
        } else {
            Err(CoreError::FileChanged)
        }
    }
}

impl std::fmt::Debug for FileDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileDecoder")
            .field("path", &self.path)
            .field("track_id", &self.track_id)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

/// File decoder plus the same normalized framing contract used by live capture.
pub struct NormalizedFileDecoder {
    decoder: FileDecoder,
    normalizer: PcmNormalizer,
    pending: VecDeque<AudioFrame>,
    finished: bool,
}

impl NormalizedFileDecoder {
    pub fn open(
        path: &Path,
        expected_identity: Option<&FileIdentity>,
        resume_sample: u64,
    ) -> Result<Self, CoreError> {
        let decoder = FileDecoder::open(path, expected_identity)?;
        let normalizer = PcmNormalizer::for_file_resume(
            decoder.sample_rate(),
            decoder.channels(),
            resume_sample,
        )?;
        Ok(Self {
            decoder,
            normalizer,
            pending: VecDeque::new(),
            finished: false,
        })
    }

    pub fn next_frame(&mut self) -> Result<Option<AudioFrame>, CoreError> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        if self.finished {
            return Ok(None);
        }
        loop {
            match self.decoder.decode_next()? {
                Some(chunk) => {
                    self.pending
                        .extend(self.normalizer.push_interleaved(&chunk.samples)?);
                }
                None => {
                    self.pending.extend(self.normalizer.finish()?);
                    self.finished = true;
                }
            }
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            if self.finished {
                return Ok(None);
            }
        }
    }
}

fn map_decode_error(error: SymphoniaError) -> CoreError {
    CoreError::AudioDecode(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{f32::consts::TAU, io::Write};

    use tempfile::NamedTempFile;

    use super::*;

    fn wav_file(sample_rate: u32, channels: u16, frames: u32) -> NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let bytes_per_sample = 2_u16;
        let data_length = frames * u32::from(channels) * u32::from(bytes_per_sample);
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bytes_per_sample);
        let block_align = channels * bytes_per_sample;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + data_length).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt ");
        header.extend_from_slice(&16_u32.to_le_bytes());
        header.extend_from_slice(&1_u16.to_le_bytes());
        header.extend_from_slice(&channels.to_le_bytes());
        header.extend_from_slice(&sample_rate.to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&block_align.to_le_bytes());
        header.extend_from_slice(&(bytes_per_sample * 8).to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&data_length.to_le_bytes());
        file.write_all(&header).unwrap();
        for frame in 0..frames {
            let value = ((frame as f32 * 440.0 * TAU / sample_rate as f32).sin()
                * f32::from(i16::MAX)
                * 0.25) as i16;
            for _ in 0..channels {
                file.write_all(&value.to_le_bytes()).unwrap();
            }
        }
        file.flush().unwrap();
        file
    }

    #[test]
    fn decodes_and_normalizes_a_wav_without_loading_the_whole_file() {
        let file = wav_file(48_000, 2, 48_000);
        let identity = FileIdentity::from_file(file.as_file()).unwrap();
        let mut decoder = NormalizedFileDecoder::open(file.path(), Some(&identity), 0).unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().unwrap() {
            frames.push(frame);
        }
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.samples.len())
                .sum::<usize>(),
            16_000
        );
        assert!(frames.iter().all(|frame| frame.samples.len() <= 512));
    }

    #[test]
    fn extension_filter_is_one_explicit_supported_set() {
        assert_eq!(
            SUPPORTED_AUDIO_EXTENSIONS,
            [
                "aac", "aif", "aiff", "caf", "flac", "m4a", "mp3", "ogg", "wav"
            ]
        );
    }
}
