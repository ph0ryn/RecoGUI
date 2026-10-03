use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

/// Read GGUF v2/v3 string metadata without loading tokenizer arrays or tensors.
/// Layout: https://github.com/ggml-org/ggml/blob/master/docs/gguf.md
pub(super) struct Metadata {
    pub strings: HashMap<String, String>,
    pub tags: Vec<String>,
}

pub(super) fn string_metadata(path: &Path) -> io::Result<Metadata> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut magic = [0; 4];
    reader.read_exact(&mut magic)?;
    if &magic != b"GGUF" || !matches!(u32_value(&mut reader)?, 2 | 3) {
        return Err(invalid("expected a little-endian GGUF v2/v3 file"));
    }
    let _tensors = u64_value(&mut reader)?;
    let count = u64_value(&mut reader)?;
    let mut values = HashMap::new();
    let mut tags = Vec::new();
    for _ in 0..count {
        let key = string(&mut reader)?;
        let kind = u32_value(&mut reader)?;
        if key == "general.tags" && kind == 9 {
            if u32_value(&mut reader)? != 8 {
                return Err(invalid("GGUF general.tags must contain strings"));
            }
            for _ in 0..u64_value(&mut reader)? {
                tags.push(string(&mut reader)?);
            }
        } else if kind == 8 {
            values.insert(key, string(&mut reader)?);
        } else {
            skip_value(&mut reader, kind)?;
        }
    }
    Ok(Metadata {
        strings: values,
        tags,
    })
}

fn u32_value(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn u64_value(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn string(reader: &mut impl Read) -> io::Result<String> {
    let length =
        usize::try_from(u64_value(reader)?).map_err(|_| invalid("GGUF string is too large"))?;
    if length > 1024 * 1024 {
        return Err(invalid("GGUF metadata string exceeds 1 MiB"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| invalid("GGUF metadata is not UTF-8"))
}

fn skip_value(reader: &mut (impl Read + Seek), kind: u32) -> io::Result<()> {
    let bytes = match kind {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        8 => u64_value(reader)?,
        9 => {
            let element_kind = u32_value(reader)?;
            let count = u64_value(reader)?;
            if element_kind == 9 {
                return Err(invalid("nested GGUF metadata arrays are unsupported"));
            }
            for _ in 0..count {
                skip_value(reader, element_kind)?;
            }
            return Ok(());
        }
        _ => return Err(invalid("unknown GGUF metadata value type")),
    };
    reader.seek(SeekFrom::Current(
        i64::try_from(bytes).map_err(|_| invalid("GGUF value is too large"))?,
    ))?;
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
pub(super) fn write_fixture(path: &Path, metadata: &[(&str, &str)]) {
    let mut bytes = b"GGUF".to_vec();
    bytes.extend(3u32.to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    bytes.extend((metadata.len() as u64 + 1).to_le_bytes());
    for (key, value) in metadata {
        bytes.extend((key.len() as u64).to_le_bytes());
        bytes.extend(key.as_bytes());
        bytes.extend(8u32.to_le_bytes());
        bytes.extend((value.len() as u64).to_le_bytes());
        bytes.extend(value.as_bytes());
    }
    let key = "general.tags";
    let tag = "automatic-speech-recognition";
    bytes.extend((key.len() as u64).to_le_bytes());
    bytes.extend(key.as_bytes());
    bytes.extend(9u32.to_le_bytes());
    bytes.extend(8u32.to_le_bytes());
    bytes.extend(1u64.to_le_bytes());
    bytes.extend((tag.len() as u64).to_le_bytes());
    bytes.extend(tag.as_bytes());
    std::fs::write(path, bytes).unwrap();
}
