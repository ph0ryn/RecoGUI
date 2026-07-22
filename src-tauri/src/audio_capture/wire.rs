use serde::Serialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use super::{AudioCaptureError, AudioFrame, OUTPUT_FRAME_SAMPLES, OUTPUT_SAMPLE_RATE};

pub const HEADER_BYTES: usize = 64;
pub const WIRE_VERSION: u16 = 1;
pub const SAMPLE_FORMAT_F32_LE: u16 = 1;
pub const MAX_ERROR_PAYLOAD_BYTES: usize = 4_096;
const MAGIC: [u8; 4] = *b"RPCM";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum RecordKind {
    Start = 1,
    Data = 2,
    End = 3,
    Error = 4,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RecordHeader {
    pub kind: RecordKind,
    pub session_id: Uuid,
    pub generation: u32,
    pub sequence: u64,
    pub start_sample: u64,
    pub sample_count: u32,
    pub payload_length: u32,
}

impl RecordHeader {
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut encoded = [0; HEADER_BYTES];
        encoded[0..4].copy_from_slice(&MAGIC);
        encoded[4..6].copy_from_slice(&WIRE_VERSION.to_le_bytes());
        encoded[6..8].copy_from_slice(&(self.kind as u16).to_le_bytes());
        encoded[8..24].copy_from_slice(self.session_id.as_bytes());
        encoded[24..28].copy_from_slice(&self.generation.to_le_bytes());
        encoded[28..36].copy_from_slice(&self.sequence.to_le_bytes());
        encoded[36..44].copy_from_slice(&self.start_sample.to_le_bytes());
        encoded[44..48].copy_from_slice(&OUTPUT_SAMPLE_RATE.to_le_bytes());
        encoded[48..50].copy_from_slice(&1_u16.to_le_bytes());
        encoded[50..52].copy_from_slice(&SAMPLE_FORMAT_F32_LE.to_le_bytes());
        encoded[52..56].copy_from_slice(&self.sample_count.to_le_bytes());
        encoded[56..60].copy_from_slice(&self.payload_length.to_le_bytes());
        encoded[60..64].copy_from_slice(&0_u32.to_le_bytes());
        encoded
    }
}

pub struct AudioWireWriter<W> {
    output: W,
    session_id: Uuid,
    generation: u32,
    sequence: u64,
    next_start_sample: u64,
    started: bool,
    terminal: bool,
    partial_frame_written: bool,
}

impl<W> AudioWireWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(output: W, session_id: Uuid, generation: u32, start_sample: u64) -> Self {
        Self {
            output,
            session_id,
            generation,
            sequence: 0,
            next_start_sample: start_sample,
            started: false,
            terminal: false,
            partial_frame_written: false,
        }
    }

    pub async fn write_start(&mut self) -> std::io::Result<()> {
        if self.started || self.terminal || self.sequence != 0 {
            return Err(invalid_state("START must be the first record"));
        }
        self.write_record(RecordKind::Start, self.next_start_sample, 0, &[])
            .await?;
        self.started = true;
        Ok(())
    }

    pub async fn write_frame(&mut self, frame: &AudioFrame) -> std::io::Result<()> {
        if !self.started || self.terminal {
            return Err(invalid_state("DATA requires an active stream"));
        }
        if frame.samples.is_empty() || frame.samples.len() > OUTPUT_FRAME_SAMPLES {
            return Err(invalid_state("DATA sample count must be between 1 and 512"));
        }
        if self.partial_frame_written {
            return Err(invalid_state("DATA cannot follow a partial final frame"));
        }
        if frame.start_sample != self.next_start_sample {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "non-contiguous audio frame: expected {}, got {}",
                    self.next_start_sample, frame.start_sample
                ),
            ));
        }
        let mut payload = Vec::with_capacity(frame.samples.len() * size_of::<f32>());
        for sample in &frame.samples {
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        self.write_record(
            RecordKind::Data,
            frame.start_sample,
            frame.samples.len() as u32,
            &payload,
        )
        .await?;
        self.next_start_sample += frame.samples.len() as u64;
        self.partial_frame_written = frame.samples.len() < OUTPUT_FRAME_SAMPLES;
        Ok(())
    }

    pub async fn write_end(&mut self) -> std::io::Result<()> {
        if !self.started || self.terminal {
            return Err(invalid_state("END requires an active stream"));
        }
        self.write_record(RecordKind::End, self.next_start_sample, 0, &[])
            .await?;
        self.terminal = true;
        Ok(())
    }

    pub async fn write_error(&mut self, error: &AudioCaptureError) -> std::io::Result<()> {
        if self.terminal {
            return Err(invalid_state("ERROR cannot follow a terminal record"));
        }
        #[derive(Serialize)]
        struct ErrorPayload<'a> {
            code: &'a str,
            message: String,
        }
        let payload = serde_json::to_vec(&ErrorPayload {
            code: error_code(error),
            message: error.to_string(),
        })
        .map_err(std::io::Error::other)?;
        if payload.is_empty() || payload.len() > MAX_ERROR_PAYLOAD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "audio error payload exceeds 4 KiB",
            ));
        }
        self.write_record(RecordKind::Error, self.next_start_sample, 0, &payload)
            .await?;
        self.terminal = true;
        Ok(())
    }

    async fn write_record(
        &mut self,
        kind: RecordKind,
        start_sample: u64,
        sample_count: u32,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let header = RecordHeader {
            kind,
            session_id: self.session_id,
            generation: self.generation,
            sequence: self.sequence,
            start_sample,
            sample_count,
            payload_length: payload.len() as u32,
        };
        self.output.write_all(&header.encode()).await?;
        self.output.write_all(payload).await?;
        self.output.flush().await?;
        self.sequence += 1;
        Ok(())
    }
}

fn invalid_state(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn error_code(error: &AudioCaptureError) -> &'static str {
    match error {
        AudioCaptureError::RingOverflow => "AUDIO_RING_OVERFLOW",
        AudioCaptureError::DeviceNotFound(_) | AudioCaptureError::NoInputDevice => {
            "AUDIO_DEVICE_NOT_FOUND"
        }
        AudioCaptureError::MicrophonePermissionDenied
        | AudioCaptureError::SystemAudioPermissionDenied => "AUDIO_PERMISSION_DENIED",
        AudioCaptureError::PermissionPromptTimedOut => "AUDIO_PERMISSION_TIMEOUT",
        #[cfg(not(target_os = "macos"))]
        AudioCaptureError::UnsupportedOperatingSystem => "AUDIO_OS_UNSUPPORTED",
        AudioCaptureError::SelfExclusionUnavailable => "AUDIO_SELF_EXCLUSION_UNAVAILABLE",
        AudioCaptureError::Protocol(_) => "AUDIO_PROTOCOL_ERROR",
        _ => "AUDIO_CAPTURE_FAILED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn header_matches_the_python_wire_layout() {
        let session_id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let header = RecordHeader {
            kind: RecordKind::Data,
            session_id,
            generation: 7,
            sequence: 9,
            start_sample: 123,
            sample_count: 512,
            payload_length: 2_048,
        }
        .encode();
        assert_eq!(&header[0..4], b"RPCM");
        assert_eq!(&header[8..24], session_id.as_bytes());
        assert_eq!(u32::from_le_bytes(header[24..28].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(header[28..36].try_into().unwrap()), 9);
        assert_eq!(
            u32::from_le_bytes(header[44..48].try_into().unwrap()),
            16_000
        );
        assert_eq!(u16::from_le_bytes(header[48..50].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(header[50..52].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(header[60..64].try_into().unwrap()), 0);
    }

    #[tokio::test]
    async fn writer_emits_contiguous_start_data_end_records() {
        let (output, mut input) = tokio::io::duplex(8_192);
        let session_id = Uuid::new_v4();
        let task = tokio::spawn(async move {
            let mut writer = AudioWireWriter::new(output, session_id, 2, 10);
            writer.write_start().await.unwrap();
            writer
                .write_frame(&AudioFrame {
                    start_sample: 10,
                    samples: vec![0.25; 12],
                })
                .await
                .unwrap();
            writer.write_end().await.unwrap();
        });
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes).await.unwrap();
        task.await.unwrap();
        assert_eq!(bytes.len(), HEADER_BYTES * 3 + 12 * 4);
        assert_eq!(u64::from_le_bytes(bytes[28..36].try_into().unwrap()), 0);
        let second = HEADER_BYTES;
        assert_eq!(
            u64::from_le_bytes(bytes[second + 28..second + 36].try_into().unwrap()),
            1
        );
        let third = HEADER_BYTES * 2 + 12 * 4;
        assert_eq!(
            u64::from_le_bytes(bytes[third + 28..third + 36].try_into().unwrap()),
            2
        );
    }

    #[tokio::test]
    async fn writer_rejects_empty_and_oversized_data() {
        for samples in [Vec::new(), vec![0.0; 513]] {
            let (output, _input) = tokio::io::duplex(8_192);
            let mut writer = AudioWireWriter::new(output, Uuid::new_v4(), 0, 0);
            writer.write_start().await.unwrap();
            assert!(
                writer
                    .write_frame(&AudioFrame {
                        start_sample: 0,
                        samples,
                    })
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn writer_allows_only_terminal_after_partial_data() {
        let (output, _input) = tokio::io::duplex(8_192);
        let mut writer = AudioWireWriter::new(output, Uuid::new_v4(), 0, 0);
        writer.write_start().await.unwrap();
        writer
            .write_frame(&AudioFrame {
                start_sample: 0,
                samples: vec![0.0; 12],
            })
            .await
            .unwrap();
        assert!(
            writer
                .write_frame(&AudioFrame {
                    start_sample: 12,
                    samples: vec![0.0; 1],
                })
                .await
                .is_err()
        );
        writer.write_end().await.unwrap();
    }
}
