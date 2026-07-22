use std::{fs::File, io::Read, path::Path, sync::OnceLock};

use ort::{session::Session, value::Tensor};
use sha2::{Digest, Sha256};

use crate::application_core::{
    domain::{NORMALIZED_SAMPLE_RATE, VAD_FRAME_SAMPLES},
    error::CoreError,
    vad::ProbabilityModel,
};

pub const SILERO_VAD_SHA256: &str =
    "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3";
const CONTEXT_SAMPLES: usize = 64;
const STATE_SAMPLES: usize = 2 * 128;
static ORT_INITIALIZED: OnceLock<bool> = OnceLock::new();

pub fn validate_silero_asset(path: &Path) -> Result<(), CoreError> {
    let mut source = File::open(path).map_err(|_| CoreError::InvalidVadAsset)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|_| CoreError::InvalidVadAsset)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual == SILERO_VAD_SHA256 {
        Ok(())
    } else {
        Err(CoreError::InvalidVadAsset)
    }
}

/// Stateful Silero v6 ONNX inference with CPU-only ONNX Runtime.
pub struct SileroOnnx {
    session: Session,
    state: [f32; STATE_SAMPLES],
    context: [f32; CONTEXT_SAMPLES],
}

impl SileroOnnx {
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        validate_silero_asset(path)?;
        if !*ORT_INITIALIZED.get_or_init(|| ort::init().commit()) {
            return Err(CoreError::Vad(
                "ONNX Runtime global initialization failed".into(),
            ));
        }
        let session = Session::builder()
            .map_err(|error| CoreError::Vad(error.to_string()))?
            .commit_from_file(path)
            .map_err(|error| CoreError::Vad(error.to_string()))?;
        let inputs: Vec<_> = session.inputs().iter().map(|value| value.name()).collect();
        let outputs: Vec<_> = session.outputs().iter().map(|value| value.name()).collect();
        if inputs != ["input", "state", "sr"] || outputs != ["output", "stateN"] {
            return Err(CoreError::Vad(format!(
                "unexpected Silero graph interface: inputs={inputs:?}, outputs={outputs:?}"
            )));
        }
        Ok(Self {
            session,
            state: [0.0; STATE_SAMPLES],
            context: [0.0; CONTEXT_SAMPLES],
        })
    }
}

impl ProbabilityModel for SileroOnnx {
    fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
    }

    fn probability(&mut self, samples: &[f32; VAD_FRAME_SAMPLES]) -> Result<f32, CoreError> {
        let mut input = Vec::with_capacity(CONTEXT_SAMPLES + VAD_FRAME_SAMPLES);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(samples);
        let outputs = self
            .session
            .run(ort::inputs! {
                "input" => Tensor::from_array(([1usize, CONTEXT_SAMPLES + VAD_FRAME_SAMPLES], input))
                    .map_err(|error| CoreError::Vad(error.to_string()))?,
                "state" => Tensor::from_array(([2usize, 1usize, 128usize], self.state.to_vec()))
                    .map_err(|error| CoreError::Vad(error.to_string()))?,
                "sr" => Tensor::from_array(((), vec![i64::from(NORMALIZED_SAMPLE_RATE)]))
                    .map_err(|error| CoreError::Vad(error.to_string()))?,
            })
            .map_err(|error| CoreError::Vad(error.to_string()))?;
        let probability = {
            let (_, values) = outputs["output"]
                .try_extract_tensor::<f32>()
                .map_err(|error| CoreError::Vad(error.to_string()))?;
            *values
                .first()
                .ok_or_else(|| CoreError::Vad("Silero probability output is empty".into()))?
        };
        {
            let (_, values) = outputs["stateN"]
                .try_extract_tensor::<f32>()
                .map_err(|error| CoreError::Vad(error.to_string()))?;
            if values.len() != STATE_SAMPLES {
                return Err(CoreError::Vad(format!(
                    "Silero state output has {} values instead of {STATE_SAMPLES}",
                    values.len()
                )));
            }
            self.state.copy_from_slice(values);
        }
        self.context
            .copy_from_slice(&samples[VAD_FRAME_SAMPLES - CONTEXT_SAMPLES..]);
        if probability.is_finite() {
            Ok(probability)
        } else {
            Err(CoreError::Vad(
                "Silero returned a non-finite probability".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/models/silero_vad.onnx")
    }

    #[test]
    fn bundled_asset_matches_the_pinned_hash() {
        validate_silero_asset(&asset()).unwrap();
    }

    #[test]
    fn onnx_probability_matches_the_python_runtime_probe() {
        let mut model = SileroOnnx::load(&asset()).unwrap();
        let probability = model.probability(&[0.0; VAD_FRAME_SAMPLES]).unwrap();
        assert!((probability - 0.001_669_824_1).abs() <= 1e-7);
    }
}
