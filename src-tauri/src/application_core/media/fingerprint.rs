use std::{
    fs::{File, Metadata},
    io::Read,
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::application_core::error::CoreError;

const FINGERPRINT_BLOCK_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_ns: i128,
}

impl FileIdentity {
    pub fn from_file(file: &File) -> Result<Self, CoreError> {
        Self::from_metadata(&file.metadata()?)
    }

    fn from_metadata(metadata: &Metadata) -> Result<Self, CoreError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec()),
            })
        }

        #[cfg(not(unix))]
        {
            use std::time::UNIX_EPOCH;

            let modified_ns = metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_err(|error| CoreError::InvalidArgument(error.to_string()))?
                .as_nanos();
            Ok(Self {
                device: 0,
                inode: 0,
                size: metadata.len(),
                modified_ns: i128::try_from(modified_ns).map_err(|_| {
                    CoreError::InvalidArgument("file timestamp exceeds supported range".into())
                })?,
            })
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFingerprint {
    pub value: String,
    pub identity: FileIdentity,
}

pub fn fingerprint_file(path: &Path) -> Result<FileFingerprint, CoreError> {
    let mut source = File::open(path)?;
    let identity = FileIdentity::from_file(&source)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; FINGERPRINT_BLOCK_SIZE];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if FileIdentity::from_file(&source)? != identity {
        return Err(CoreError::FileChanged);
    }
    Ok(FileFingerprint {
        value: format!("sha256:{:x}", digest.finalize()),
        identity,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn fingerprint_binds_the_hash_to_one_file_identity() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"RecoGUI").unwrap();
        file.flush().unwrap();

        let fingerprint = fingerprint_file(file.path()).unwrap();

        assert_eq!(
            fingerprint.value,
            "sha256:c41047ad4d8cd361d246df55a15439e651d66622a24b9b88cc008853b2490501"
        );
        assert_eq!(fingerprint.identity.size, 7);
    }
}
