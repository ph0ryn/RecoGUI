use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::application_core::error::CoreError;

const MAGIC: [u8; 4] = *b"RASR";
const HEADER_BYTES: usize = 16;
pub const RASR_PROTOCOL_VERSION: u16 = 1;
pub const MAX_JSON_BYTES: usize = 64 * 1024;
pub const MAX_BINARY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum FrameKind {
    Hello = 1,
    Request = 2,
    Response = 3,
    Heartbeat = 4,
}

impl FrameKind {
    fn parse(value: u16) -> Result<Self, CoreError> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Request),
            3 => Ok(Self::Response),
            4 => Ok(Self::Heartbeat),
            _ => Err(CoreError::WorkerProtocol(format!(
                "unknown RASR frame kind: {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RasrFrame {
    pub kind: FrameKind,
    pub metadata: Value,
    pub binary: Vec<u8>,
}

impl RasrFrame {
    pub fn metadata_as<T: DeserializeOwned>(&self) -> Result<T, CoreError> {
        serde_json::from_value(self.metadata.clone())
            .map_err(|error| CoreError::WorkerProtocol(error.to_string()))
    }
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<RasrFrame, CoreError> {
    let mut header = [0_u8; HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe => {
                CoreError::WorkerClosed
            }
            _ => CoreError::Io(error),
        })?;
    let (kind, json_length, binary_length) = decode_header(header)?;
    let mut json = vec![0_u8; json_length];
    reader.read_exact(&mut json).await.map_err(map_body_error)?;
    let metadata: Value = serde_json::from_slice(&json)
        .map_err(|error| CoreError::WorkerProtocol(format!("invalid RASR JSON: {error}")))?;
    if !metadata.is_object() {
        return Err(CoreError::WorkerProtocol(
            "RASR metadata must be a JSON object".into(),
        ));
    }
    let mut binary = vec![0_u8; binary_length];
    reader
        .read_exact(&mut binary)
        .await
        .map_err(map_body_error)?;
    Ok(RasrFrame {
        kind,
        metadata,
        binary,
    })
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    kind: FrameKind,
    metadata: &T,
    binary: &[u8],
) -> Result<(), CoreError> {
    let json = serde_json::to_vec(metadata)
        .map_err(|error| CoreError::WorkerProtocol(error.to_string()))?;
    validate_lengths(json.len(), binary.len())?;
    let header = encode_header(kind, json.len(), binary.len())?;
    writer.write_all(&header).await?;
    writer.write_all(&json).await?;
    writer.write_all(binary).await?;
    writer.flush().await?;
    Ok(())
}

fn decode_header(header: [u8; HEADER_BYTES]) -> Result<(FrameKind, usize, usize), CoreError> {
    if header[..4] != MAGIC {
        return Err(CoreError::WorkerProtocol("invalid RASR magic".into()));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != RASR_PROTOCOL_VERSION {
        return Err(CoreError::WorkerProtocol(format!(
            "unsupported RASR version: {version}"
        )));
    }
    let kind = FrameKind::parse(u16::from_le_bytes([header[6], header[7]]))?;
    let json_length =
        u32::from_le_bytes(header[8..12].try_into().expect("fixed header slice")) as usize;
    let binary_length =
        u32::from_le_bytes(header[12..16].try_into().expect("fixed header slice")) as usize;
    validate_lengths(json_length, binary_length)?;
    Ok((kind, json_length, binary_length))
}

fn encode_header(
    kind: FrameKind,
    json_length: usize,
    binary_length: usize,
) -> Result<[u8; HEADER_BYTES], CoreError> {
    validate_lengths(json_length, binary_length)?;
    let json_length = u32::try_from(json_length)
        .map_err(|_| CoreError::WorkerProtocol("RASR JSON length overflows u32".into()))?;
    let binary_length = u32::try_from(binary_length)
        .map_err(|_| CoreError::WorkerProtocol("RASR binary length overflows u32".into()))?;
    let mut header = [0_u8; HEADER_BYTES];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&RASR_PROTOCOL_VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    header[8..12].copy_from_slice(&json_length.to_le_bytes());
    header[12..16].copy_from_slice(&binary_length.to_le_bytes());
    Ok(header)
}

fn validate_lengths(json_length: usize, binary_length: usize) -> Result<(), CoreError> {
    if json_length == 0 || json_length > MAX_JSON_BYTES {
        return Err(CoreError::WorkerProtocol(format!(
            "RASR JSON length is outside 1..={MAX_JSON_BYTES}: {json_length}"
        )));
    }
    if binary_length > MAX_BINARY_BYTES {
        return Err(CoreError::WorkerProtocol(format!(
            "RASR binary length exceeds {MAX_BINARY_BYTES}: {binary_length}"
        )));
    }
    Ok(())
}

fn map_body_error(error: std::io::Error) -> CoreError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
    ) {
        CoreError::WorkerProtocol("truncated RASR frame".into())
    } else {
        CoreError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn frame_codec_handles_fragmented_transport_reads() {
        let (mut writer, mut reader) = duplex(256);
        let expected = json!({"requestId": "request-one", "operation": "models.list"});
        let json = serde_json::to_vec(&expected).unwrap();
        let header = encode_header(FrameKind::Request, json.len(), 4).unwrap();
        let task = tokio::spawn(async move {
            for byte in header.into_iter().chain(json).chain([1, 2, 3, 4]) {
                writer.write_all(&[byte]).await.unwrap();
            }
        });

        let actual = read_frame(&mut reader).await.unwrap();
        task.await.unwrap();
        assert_eq!(actual.kind, FrameKind::Request);
        assert_eq!(actual.metadata, expected);
        assert_eq!(actual.binary, [1, 2, 3, 4]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_length_is_rejected_before_a_body_allocation_or_read() {
        let (mut writer, mut reader) = duplex(64);
        let mut header = encode_header(FrameKind::Request, 2, 0).unwrap();
        header[12..16].copy_from_slice(&((MAX_BINARY_BYTES as u32) + 1).to_le_bytes());
        writer.write_all(&header).await.unwrap();

        assert!(matches!(
            read_frame(&mut reader).await,
            Err(CoreError::WorkerProtocol(_))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_magic_version_kind_and_json_are_fatal() {
        for header in [
            {
                let mut value = encode_header(FrameKind::Hello, 2, 0).unwrap();
                value[0] = b'X';
                value
            },
            {
                let mut value = encode_header(FrameKind::Hello, 2, 0).unwrap();
                value[4..6].copy_from_slice(&2_u16.to_le_bytes());
                value
            },
            {
                let mut value = encode_header(FrameKind::Hello, 2, 0).unwrap();
                value[6..8].copy_from_slice(&99_u16.to_le_bytes());
                value
            },
        ] {
            let (mut writer, mut reader) = duplex(64);
            writer.write_all(&header).await.unwrap();
            writer.write_all(b"{}").await.unwrap();
            assert!(read_frame(&mut reader).await.is_err());
        }

        let (mut writer, mut reader) = duplex(64);
        let header = encode_header(FrameKind::Hello, 1, 0).unwrap();
        writer.write_all(&header).await.unwrap();
        writer.write_all(b"{").await.unwrap();
        assert!(read_frame(&mut reader).await.is_err());
    }
}
