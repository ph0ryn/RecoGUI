//! Qwen3-ASR combined model.
//!
//! AudioEncoder + Qwen3 LLM decoder for speech recognition.

use crate::audio::{self, AudioConfig, MelFrontend};
use crate::encoder::{AudioEncoder, AudioEncoderConfig};
use crate::error::{Error, Result};
use crate::qwen::{QwenAttention, QwenBlock, QwenConfig, QwenMLP, QwenModel, convert_rope_scaling};

use mlx_rs::Array;
use mlx_rs::macros::ModuleParameters;
use mlx_rs::module::{ModuleParameters as ModuleParametersTrait, Param};
use mlx_rs::nn;
use mlx_rs::ops::indexing::{IndexOp, argmax_axis};
use mlx_rs::quantization::MaybeQuantized;
use mlx_rs::transforms::eval;
use mlx_rs_core::{KVCache, initialize_rope};
use std::collections::HashMap;
use std::path::Path;

/// Quantization configuration.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct QuantizationConfig {
    #[serde(default = "default_group_size")]
    pub group_size: i32,
    #[serde(default = "default_bits")]
    pub bits: i32,
}

fn default_group_size() -> i32 {
    64
}
fn default_bits() -> i32 {
    4
}

/// Qwen3-ASR model configuration.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Qwen3ASRConfig {
    #[serde(default)]
    pub audio_config: AudioEncoderConfig,
    #[serde(default)]
    pub text_config: QwenConfig,
    #[serde(default = "default_audio_token_id")]
    pub audio_token_id: i32,
    #[serde(default = "default_audio_start_token_id")]
    pub audio_start_token_id: i32,
    #[serde(default = "default_audio_end_token_id")]
    pub audio_end_token_id: i32,
    #[serde(default)]
    pub support_languages: Vec<String>,
    #[serde(default)]
    pub quantization: Option<QuantizationConfig>,
}

fn default_audio_token_id() -> i32 {
    151676
}
fn default_audio_start_token_id() -> i32 {
    151669
}
fn default_audio_end_token_id() -> i32 {
    151670
}

impl Default for Qwen3ASRConfig {
    fn default() -> Self {
        Self {
            audio_config: AudioEncoderConfig::default(),
            text_config: QwenConfig::default(),
            audio_token_id: 151676,
            audio_start_token_id: 151669,
            audio_end_token_id: 151670,
            support_languages: vec![
                "Chinese".into(),
                "English".into(),
                "Cantonese".into(),
                "Japanese".into(),
                "Korean".into(),
                "French".into(),
                "German".into(),
                "Spanish".into(),
                "Russian".into(),
            ],
            quantization: None,
        }
    }
}

impl Qwen3ASRConfig {
    /// Parse from config.json, handling the thinker_config nesting.
    pub fn from_config_json(value: &serde_json::Value) -> Result<Self> {
        let thinker = value.get("thinker_config").unwrap_or(value);

        let audio_config: AudioEncoderConfig = if let Some(ac) = thinker.get("audio_config") {
            serde_json::from_value(ac.clone())?
        } else {
            AudioEncoderConfig::default()
        };

        let text_config: QwenConfig = if let Some(tc) = thinker.get("text_config") {
            serde_json::from_value(tc.clone())?
        } else {
            QwenConfig::default()
        };

        let audio_token_id = thinker
            .get("audio_token_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(151676);

        let audio_start_token_id = thinker
            .get("audio_start_token_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(151669);

        let audio_end_token_id = thinker
            .get("audio_end_token_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(151670);

        let support_languages = value
            .get("support_languages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Quantization is at top level
        let quantization: Option<QuantizationConfig> = value
            .get("quantization")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        Ok(Self {
            audio_config,
            text_config,
            audio_token_id,
            audio_start_token_id,
            audio_end_token_id,
            support_languages,
            quantization,
        })
    }
}

/// Sampling configuration for text generation.
#[derive(Debug, Clone)]
pub struct SamplingConfig {
    pub temperature: f32,
    pub max_tokens: usize,
    pub repetition_penalty: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct TranscriptionOutput {
    pub text: String,
    pub language: String,
    pub generation_tokens: u32,
    pub prompt_tokens: u32,
    pub token_limit_reached: bool,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            temperature: 0.0, // Greedy
            max_tokens: 8192,
            repetition_penalty: None,
        }
    }
}

/// Qwen3-ASR model.
#[derive(ModuleParameters)]
pub struct Qwen3ASR {
    /// Audio encoder (Conv2d + Transformer)
    #[param]
    pub audio_tower: AudioEncoder,

    /// Text decoder (Qwen3)
    #[param]
    pub model: QwenModel,

    /// Model configuration
    pub config: Qwen3ASRConfig,

    /// Audio frontend
    mel_frontend: MelFrontend,

    /// Tokenizer
    tokenizer: Option<tokenizers::Tokenizer>,

    /// EOS token IDs for stopping generation
    eos_token_ids: Vec<i32>,
}

// ============================================================================
// Weight loading helpers
// ============================================================================

fn get_weight(weights: &HashMap<String, Array>, key: &str) -> Result<Array> {
    weights
        .get(key)
        .cloned()
        .ok_or_else(|| Error::Weight(format!("Weight not found: {}", key)))
}

fn make_quantized_linear(
    weights: &HashMap<String, Array>,
    prefix: &str,
    group_size: i32,
    bits: i32,
) -> Result<nn::QuantizedLinear> {
    let weight = get_weight(weights, &format!("{}.weight", prefix))?;
    let scales = get_weight(weights, &format!("{}.scales", prefix))?;
    let biases = get_weight(weights, &format!("{}.biases", prefix))?;

    let inner = nn::Linear {
        weight: Param::new(weight),
        bias: Param::new(None),
    };

    let mut ql = nn::QuantizedLinear {
        group_size,
        bits,
        scales: Param::new(scales),
        biases: Param::new(biases),
        inner,
    };
    ql.freeze_parameters(true);
    Ok(ql)
}

fn make_quantized_embedding(
    weights: &HashMap<String, Array>,
    prefix: &str,
    group_size: i32,
    bits: i32,
) -> Result<nn::QuantizedEmbedding> {
    let weight = get_weight(weights, &format!("{}.weight", prefix))?;
    let scales = get_weight(weights, &format!("{}.scales", prefix))?;
    let biases = get_weight(weights, &format!("{}.biases", prefix))?;

    let inner = nn::Embedding {
        weight: Param::new(weight),
    };

    let mut qe = nn::QuantizedEmbedding {
        group_size,
        bits,
        scales: Param::new(scales),
        biases: Param::new(biases),
        inner,
    };
    qe.freeze_parameters(true);
    Ok(qe)
}

/// Load all safetensors weights from a model directory.
fn load_all_weights(model_dir: &Path) -> Result<HashMap<String, Array>> {
    let single_file = model_dir.join("model.safetensors");
    if single_file.exists() {
        let loaded = Array::load_safetensors(&single_file)
            .map_err(|e| Error::ModelLoad(format!("Failed to load safetensors: {}", e)))?;
        return Ok(loaded);
    }

    // Try sharded files: model-00001-of-NNNNN.safetensors
    let mut all_weights = HashMap::new();
    let mut shard_idx = 1;
    loop {
        let shard_name = format!("model-{:05}-of-", shard_idx);
        let shard_files: Vec<_> = std::fs::read_dir(model_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(&shard_name) && name.ends_with(".safetensors")
            })
            .collect();

        if shard_files.is_empty() {
            break;
        }

        for entry in shard_files {
            let loaded = Array::load_safetensors(&entry.path()).map_err(|e| {
                Error::ModelLoad(format!("Failed to load {}: {}", entry.path().display(), e))
            })?;
            all_weights.extend(loaded);
        }
        shard_idx += 1;
    }

    if all_weights.is_empty() {
        return Err(Error::ModelLoad(format!(
            "No safetensors files found in {}",
            model_dir.display()
        )));
    }

    Ok(all_weights)
}

impl Qwen3ASR {
    /// Load model from directory.
    pub fn load(model_dir: impl AsRef<Path>) -> Result<Self> {
        let model_dir = model_dir.as_ref();

        // Load config.json
        let config_path = model_dir.join("config.json");
        if !config_path.exists() {
            return Err(Error::ModelLoad(format!(
                "config.json not found at {}",
                config_path.display()
            )));
        }
        let config_json: serde_json::Value = {
            let file = std::fs::File::open(&config_path)?;
            serde_json::from_reader(file)?
        };
        let config = Qwen3ASRConfig::from_config_json(&config_json)?;

        eprintln!(
            "Audio encoder: {} layers, d_model={}",
            config.audio_config.encoder_layers, config.audio_config.d_model
        );
        eprintln!(
            "Text decoder: {} layers, hidden_size={}",
            config.text_config.num_hidden_layers, config.text_config.hidden_size
        );

        if let Some(ref qc) = config.quantization {
            eprintln!("Quantization: {}bit, group_size={}", qc.bits, qc.group_size);
        }

        // Load all weights
        eprintln!("Loading weights...");
        let weights = load_all_weights(model_dir)?;
        eprintln!("Loaded {} weight tensors", weights.len());

        // Build audio encoder (NOT quantized) and load its weights
        let mut audio_tower = AudioEncoder::new(config.audio_config.clone())?;
        {
            let mut params = audio_tower.parameters_mut().flatten();
            let mut loaded = 0;
            for (key, value) in &weights {
                if key.starts_with("audio_tower.") {
                    let param_key = &key["audio_tower.".len()..];
                    if let Some(param) = params.get_mut(param_key) {
                        **param = value.clone();
                        loaded += 1;
                    }
                }
            }
            let expected = params.len();
            eprintln!("Audio tower: loaded {}/{} parameters", loaded, expected);
            if loaded < expected {
                return Err(Error::ModelLoad(format!(
                    "{} audio tower parameters are missing",
                    expected - loaded
                )));
            }
            eval(params.values().map(|v| &**v))?;
        }

        // Build text decoder
        let text_model = if let Some(ref qc) = config.quantization {
            Self::build_quantized_text_model(&config.text_config, &weights, qc.group_size, qc.bits)?
        } else {
            Self::build_text_model(&config.text_config, &weights)?
        };

        // Load tokenizer and EOS tokens
        let tokenizer = Self::load_tokenizer(model_dir)?;
        let eos_token_ids = Self::parse_eos_tokens(model_dir)?;

        let mel_frontend = MelFrontend::new(AudioConfig::default());

        Ok(Self {
            audio_tower,
            model: text_model,
            config,
            mel_frontend,
            tokenizer,
            eos_token_ids,
        })
    }

    /// Build quantized text decoder from weight HashMap.
    fn build_quantized_text_model(
        config: &QwenConfig,
        weights: &HashMap<String, Array>,
        group_size: i32,
        bits: i32,
    ) -> Result<QwenModel> {
        let mut layers = Vec::with_capacity(config.num_hidden_layers as usize);

        let rope_scaling_map = convert_rope_scaling(&config.rope_scaling);

        for i in 0..config.num_hidden_layers {
            let prefix = format!("model.layers.{}", i);

            let attention = QwenAttention {
                n_heads: config.num_attention_heads,
                n_kv_heads: config.num_key_value_heads,
                head_dim: config.head_dim,
                scale: (config.head_dim as f32).powf(-0.5),
                q_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.self_attn.q_proj", prefix),
                    group_size,
                    bits,
                )?),
                k_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.self_attn.k_proj", prefix),
                    group_size,
                    bits,
                )?),
                v_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.self_attn.v_proj", prefix),
                    group_size,
                    bits,
                )?),
                o_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.self_attn.o_proj", prefix),
                    group_size,
                    bits,
                )?),
                q_norm: nn::RmsNorm {
                    weight: Param::new(get_weight(
                        weights,
                        &format!("{}.self_attn.q_norm.weight", prefix),
                    )?),
                    eps: config.rms_norm_eps,
                },
                k_norm: nn::RmsNorm {
                    weight: Param::new(get_weight(
                        weights,
                        &format!("{}.self_attn.k_norm.weight", prefix),
                    )?),
                    eps: config.rms_norm_eps,
                },
                rope: initialize_rope(
                    config.head_dim,
                    config.rope_theta,
                    false,
                    &rope_scaling_map,
                    config.max_position_embeddings,
                )?,
            };

            let mlp = QwenMLP {
                gate_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.mlp.gate_proj", prefix),
                    group_size,
                    bits,
                )?),
                down_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.mlp.down_proj", prefix),
                    group_size,
                    bits,
                )?),
                up_proj: MaybeQuantized::Quantized(make_quantized_linear(
                    weights,
                    &format!("{}.mlp.up_proj", prefix),
                    group_size,
                    bits,
                )?),
            };

            let block = QwenBlock {
                self_attn: attention,
                mlp,
                input_layernorm: nn::RmsNorm {
                    weight: Param::new(get_weight(
                        weights,
                        &format!("{}.input_layernorm.weight", prefix),
                    )?),
                    eps: config.rms_norm_eps,
                },
                post_attention_layernorm: nn::RmsNorm {
                    weight: Param::new(get_weight(
                        weights,
                        &format!("{}.post_attention_layernorm.weight", prefix),
                    )?),
                    eps: config.rms_norm_eps,
                },
            };

            layers.push(block);
        }

        let qwen_model = QwenModel {
            embed_tokens: MaybeQuantized::Quantized(make_quantized_embedding(
                weights,
                "model.embed_tokens",
                group_size,
                bits,
            )?),
            layers,
            norm: nn::RmsNorm {
                weight: Param::new(get_weight(weights, "model.norm.weight")?),
                eps: config.rms_norm_eps,
            },
            config: config.clone(),
        };

        eprintln!(
            "Text decoder: loaded {} quantized layers",
            config.num_hidden_layers
        );

        // Eval all text model params
        let params = qwen_model.parameters().flatten();
        eval(params.values().copied())?;

        Ok(qwen_model)
    }

    /// Build non-quantized text decoder from weight HashMap (fallback).
    fn build_text_model(
        config: &QwenConfig,
        weights: &HashMap<String, Array>,
    ) -> Result<QwenModel> {
        let mut model = QwenModel::new(config.clone())?;
        let mut params = model.parameters_mut().flatten();
        let mut loaded = 0;
        for (key, value) in weights {
            if key.starts_with("model.") {
                // Parameter paths from flatten() are relative to QwenModel,
                // but safetensors keys include "model." prefix.
                // Flatten keys: embed_tokens.weight, layers.0.self_attn.q_proj.weight, ...
                // Safetensors keys: model.embed_tokens.weight, model.layers.0..., ...
                let param_key = &key["model.".len()..];
                if let Some(param) = params.get_mut(param_key) {
                    **param = value.clone();
                    loaded += 1;
                }
            }
        }
        let expected = params.len();
        eprintln!(
            "Text decoder: loaded {}/{} parameters (non-quantized)",
            loaded, expected
        );
        if loaded < expected {
            return Err(Error::ModelLoad(format!(
                "{} text decoder parameters are missing",
                expected - loaded
            )));
        }
        eval(params.values().map(|v| &**v))?;
        Ok(model)
    }

    /// Load the tokenizer without modifying the Hugging Face cache.
    fn load_tokenizer(model_dir: &Path) -> Result<Option<tokenizers::Tokenizer>> {
        let tokenizer_path = model_dir.join("tokenizer.json");
        if !tokenizer_path.is_file() {
            return Err(Error::Tokenizer(format!(
                "tokenizer.json not found at {}",
                tokenizer_path.display()
            )));
        }
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|error| Error::Tokenizer(error.to_string()))?;
        Ok(Some(tokenizer))
    }

    /// Parse EOS token IDs from generation_config.json and validate the
    /// tokenizer_config.json EOS mapping.
    ///
    /// The generation configuration is the source of truth because Qwen3-ASR
    /// uses both `<|endoftext|>` and `<|im_end|>` as EOS tokens. The tokenizer
    /// configuration is still checked so a broken tokenizer cannot silently
    /// produce an unrelated EOS list.
    fn parse_eos_tokens(model_dir: &Path) -> Result<Vec<i32>> {
        let config_path = model_dir.join("tokenizer_config.json");
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
        let added_tokens = config
            .get("added_tokens_decoder")
            .and_then(|value| value.as_object())
            .ok_or_else(|| Error::Tokenizer("added_tokens_decoder is missing".into()))?;
        let eos = config
            .get("eos_token")
            .and_then(|value| value.as_str())
            .ok_or_else(|| Error::Tokenizer("eos_token is missing".into()))?;
        let id = added_tokens
            .iter()
            .find(|(_, info)| info.get("content").and_then(|value| value.as_str()) == Some(eos))
            .and_then(|(id, _)| id.parse::<i32>().ok())
            .ok_or_else(|| Error::Tokenizer(format!("EOS token {eos} has no valid ID")))?;

        let generation_path = model_dir.join("generation_config.json");
        let generation_bytes = std::fs::read(&generation_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::Tokenizer(format!(
                    "generation_config.json not found at {}",
                    generation_path.display()
                ))
            } else {
                Error::Io(error)
            }
        })?;
        let generation: serde_json::Value = serde_json::from_slice(&generation_bytes)?;
        Self::parse_generation_eos_tokens(&generation, id)
    }

    fn parse_generation_eos_tokens(
        config: &serde_json::Value,
        tokenizer_eos_id: i32,
    ) -> Result<Vec<i32>> {
        let eos_value = config
            .get("eos_token_id")
            .ok_or_else(|| Error::Tokenizer("generation_config.eos_token_id is missing".into()))?;
        let values: Vec<&serde_json::Value> = if let Some(values) = eos_value.as_array() {
            if values.is_empty() {
                return Err(Error::Tokenizer(
                    "generation_config.eos_token_id array must not be empty".into(),
                ));
            }
            values.iter().collect()
        } else {
            vec![eos_value]
        };
        let mut ids = Vec::with_capacity(values.len());
        for value in values {
            let raw = value.as_i64().ok_or_else(|| {
                Error::Tokenizer("generation_config.eos_token_id contains a non-integer".into())
            })?;
            let id = i32::try_from(raw).map_err(|_| {
                Error::Tokenizer(format!(
                    "generation_config.eos_token_id contains out-of-range ID {raw}"
                ))
            })?;
            if id < 0 {
                return Err(Error::Tokenizer(format!(
                    "generation_config.eos_token_id contains negative ID {id}"
                )));
            }
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if !ids.contains(&tokenizer_eos_id) {
            return Err(Error::Tokenizer(format!(
                "generation_config.eos_token_id does not include tokenizer EOS ID {tokenizer_eos_id}"
            )));
        }
        Ok(ids)
    }

    /// Transcribe audio file.
    pub fn transcribe(&mut self, audio_path: impl AsRef<Path>) -> Result<String> {
        self.transcribe_with_language(audio_path, "Chinese")
    }

    /// Transcribe audio file with specified language.
    pub fn transcribe_with_language(
        &mut self,
        audio_path: impl AsRef<Path>,
        language: &str,
    ) -> Result<String> {
        let (samples, sample_rate) = audio::load_wav(audio_path)?;
        let samples = audio::resample(&samples, sample_rate, 16000)?;
        self.transcribe_samples(&samples, language)
    }

    /// Transcribe audio samples (16kHz mono f32).
    /// For audio longer than 30 seconds, automatically uses chunked processing.
    pub fn transcribe_samples(&mut self, samples: &[f32], language: &str) -> Result<String> {
        let config = SamplingConfig::default();
        let chunk_threshold = 30 * 16000; // 30 seconds at 16kHz
        if samples.len() > chunk_threshold {
            self.transcribe_samples_chunked(samples, language, &config, 30.0)
        } else {
            self.transcribe_samples_with_config(samples, language, &config)
        }
    }

    /// Transcribe long audio by splitting into chunks.
    /// Each chunk is processed independently with its own KV cache.
    pub fn transcribe_samples_chunked(
        &mut self,
        samples: &[f32],
        language: &str,
        config: &SamplingConfig,
        chunk_duration_secs: f32,
    ) -> Result<String> {
        let chunk_size = (chunk_duration_secs * 16000.0) as usize;
        let total_duration = samples.len() as f32 / 16000.0;
        let num_chunks = (samples.len() + chunk_size - 1) / chunk_size;

        eprintln!(
            "Long audio: {:.1}s, splitting into {} chunks of {:.0}s",
            total_duration, num_chunks, chunk_duration_secs,
        );

        let mut transcriptions = Vec::new();

        for chunk_idx in 0..num_chunks {
            let start = chunk_idx * chunk_size;
            let end = (start + chunk_size).min(samples.len());
            let chunk_samples = &samples[start..end];
            let chunk_duration = chunk_samples.len() as f32 / 16000.0;

            // Skip very short trailing chunks
            if chunk_samples.len() < 1600 {
                continue;
            }

            eprintln!(
                "\n--- Chunk {}/{} ({:.1}s - {:.1}s, {:.1}s) ---",
                chunk_idx + 1,
                num_chunks,
                start as f32 / 16000.0,
                end as f32 / 16000.0,
                chunk_duration,
            );

            let chunk_start = std::time::Instant::now();
            match self.transcribe_samples_with_config(chunk_samples, language, config) {
                Ok(text) => {
                    let elapsed = chunk_start.elapsed().as_secs_f32();
                    let preview: String = text.chars().take(40).collect();
                    eprintln!(
                        "  -> {:.2}s ({:.1}x RT): {}{}",
                        elapsed,
                        chunk_duration / elapsed,
                        preview,
                        if text.chars().count() > 40 { "..." } else { "" }
                    );
                    if !text.is_empty() {
                        transcriptions.push(text);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Ok(transcriptions.join(""))
    }

    /// Transcribe with full configuration.
    pub fn transcribe_samples_with_config(
        &mut self,
        samples: &[f32],
        language: &str,
        config: &SamplingConfig,
    ) -> Result<String> {
        Ok(self
            .transcribe_samples_with_language(samples, Some(language), config)?
            .0)
    }

    /// Transcribe 16 kHz mono samples with optional language detection.
    pub fn transcribe_samples_detailed(
        &mut self,
        samples: &[f32],
        requested_language: Option<&str>,
        config: &SamplingConfig,
    ) -> Result<TranscriptionOutput> {
        let chunk_size = 30 * 16_000;
        let mut texts = Vec::new();
        let mut languages = Vec::new();
        let mut generation_tokens = 0u32;
        let mut prompt_tokens = 0u32;
        let mut token_limit_reached = false;
        for chunk in samples.chunks(chunk_size) {
            if chunk.len() < 1_600 {
                continue;
            }
            let (generated, chunk_generation_tokens, chunk_prompt_tokens) =
                self.transcribe_samples_with_language(chunk, requested_language, config)?;
            let (language, text) = if let Some(language) = requested_language {
                (language, generated.as_str())
            } else {
                Self::parse_auto_output(&generated)
            };
            let language = language.trim();
            if language.is_empty() {
                return Err(Error::InvalidOutput(
                    "ASR model returned an empty detected language".into(),
                ));
            }
            if !languages.iter().any(|item| item == language) {
                languages.push(language.to_owned());
            }
            texts.push(text.to_owned());
            generation_tokens += chunk_generation_tokens;
            prompt_tokens += chunk_prompt_tokens;
            token_limit_reached |= chunk_generation_tokens as usize >= config.max_tokens;
        }
        if languages.is_empty() {
            return Err(Error::audio_too_short(
                (samples.len() as u64 * 1000) / 16_000,
                audio::MIN_AUDIO_DURATION_MS,
            ));
        }
        Ok(TranscriptionOutput {
            text: texts.join(""),
            language: languages.join(", "),
            generation_tokens,
            prompt_tokens,
            token_limit_reached,
        })
    }

    fn parse_auto_output(generated: &str) -> (&str, &str) {
        let output = generated.trim();
        let Some((metadata, text)) = output.split_once("<asr_text>") else {
            // The model can emit plain text or nothing for short/silent audio.
            return ("Unknown", output);
        };
        let language = metadata
            .trim_start()
            .strip_prefix("language")
            .and_then(|value| {
                value
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
                    .then_some(value.trim_start())
            })
            .and_then(|value| value.lines().next())
            .map(str::trim)
            .filter(|language| !language.is_empty() && !language.eq_ignore_ascii_case("None"))
            .unwrap_or("Unknown");
        (language, text)
    }

    fn transcribe_samples_with_language(
        &mut self,
        samples: &[f32],
        language: Option<&str>,
        config: &SamplingConfig,
    ) -> Result<(String, u32, u32)> {
        // 1. Compute mel spectrogram (CPU-side FFT, already concrete)
        let mel = self.mel_frontend.compute_mel_spectrogram(samples)?;

        // 2. Encode audio
        let audio_features = self.audio_tower.forward_encoder(&mel)?;
        eval([&audio_features])?;

        let num_audio_tokens = audio_features.shape()[0];
        eprintln!(
            "Audio: {} mel frames -> {} audio tokens",
            mel.shape()[1],
            num_audio_tokens
        );

        // 3. Build prompt (from_slice, already concrete)
        let input_ids = self.build_prompt(num_audio_tokens, language)?;
        let prompt_tokens = input_ids.shape()[1] as u32;

        // 4. Build input embeddings with audio merged in
        let inputs_embeds = self.build_inputs_embeds(&input_ids, &audio_features)?;
        eval([&inputs_embeds])?;

        // 5. Autoregressive generation
        let (text, generation_tokens) = self.generate(&inputs_embeds, config)?;
        Ok((text, generation_tokens, prompt_tokens))
    }

    /// Transcribe several utterances in one batch.
    ///
    /// Decoding is the expensive half of an ASR request and, at batch 1, it is
    /// limited by memory bandwidth rather than compute: every step streams the
    /// full weight matrices to produce a single column of output. Running N
    /// sequences through the same steps re-uses those reads, so throughput rises
    /// far faster than step time.
    ///
    /// Prompts differ in length because utterances differ in length, so they are
    /// **left-padded** to a common length. Left rather than right padding keeps
    /// every sequence's final token at the same index, which means one shared
    /// position offset drives the whole batch and the decode loop needs no
    /// per-sequence bookkeeping. An additive mask hides the pad region.
    ///
    /// The audio encoder still runs per utterance — it is only ~26% of a request
    /// and batching it would require padding mel frames through the convolutional
    /// front-end. Decode is where the win is.
    ///
    /// Returns one transcript per input, in the order supplied.
    pub fn transcribe_batch(
        &mut self,
        batch: &[&[f32]],
        language: &str,
        config: &SamplingConfig,
    ) -> Result<Vec<String>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        if batch.len() == 1 {
            // Nothing to share — avoid the padding and masking overhead entirely.
            return Ok(vec![
                self.transcribe_samples_with_config(batch[0], language, config)?,
            ]);
        }

        // 1. Encode each utterance and build its prompt embeddings.
        let mut per_item: Vec<Array> = Vec::with_capacity(batch.len());
        for &samples in batch {
            let mel = self.mel_frontend.compute_mel_spectrogram(samples)?;
            let audio_features = self.audio_tower.forward_encoder(&mel)?;
            eval([&audio_features])?;

            let num_audio_tokens = audio_features.shape()[0];
            let input_ids = self.build_prompt(num_audio_tokens, Some(language))?;
            let embeds = self.build_inputs_embeds(&input_ids, &audio_features)?;
            per_item.push(embeds);
        }

        // 2. Left-pad to a common prompt length.
        let hidden = self.config.text_config.hidden_size;
        let max_len = per_item.iter().map(|e| e.shape()[1]).max().unwrap_or(0);
        let mut padded: Vec<Array> = Vec::with_capacity(per_item.len());
        let mut pad_lens: Vec<i32> = Vec::with_capacity(per_item.len());
        for embeds in &per_item {
            let len = embeds.shape()[1];
            let pad = max_len - len;
            pad_lens.push(pad);
            if pad == 0 {
                padded.push(embeds.clone());
            } else {
                let zeros = mlx_rs::ops::zeros_dtype(&[1, pad, hidden], embeds.dtype())?;
                padded.push(mlx_rs::ops::concatenate_axis(&[&zeros, embeds], 1)?);
            }
        }
        let refs: Vec<&Array> = padded.iter().collect();
        let inputs_embeds = mlx_rs::ops::concatenate_axis(&refs, 0)?;
        eval([&inputs_embeds])?;

        self.generate_batch(&inputs_embeds, &pad_lens, max_len, config)
    }

    /// Build the additive attention mask for a left-padded batch.
    ///
    /// `q_len` is the number of query positions in this call (the whole prompt
    /// during prefill, 1 per decode step) and `k_len` the number of keys visible
    /// so far. Blocked positions get a large negative value so softmax drives
    /// them to zero; allowed positions get 0.
    fn padding_mask(
        pad_lens: &[i32],
        q_len: i32,
        k_len: i32,
        causal_from: i32,
        dtype: mlx_rs::Dtype,
    ) -> Result<Array> {
        let b = pad_lens.len();
        let neg = -1e9f32;
        let mut data = vec![0.0f32; b * (q_len as usize) * (k_len as usize)];
        for (bi, &pad) in pad_lens.iter().enumerate() {
            for qi in 0..q_len {
                // Absolute position of this query within the padded sequence.
                let q_abs = causal_from + qi;
                for ki in 0..k_len {
                    // A query sitting in the pad region has no legal key to look
                    // at, and a row of all -inf makes softmax produce NaN — which
                    // would then spread through the batch. Always leave the
                    // diagonal open so every row stays finite. Those outputs are
                    // discarded anyway: left padding means the position we read,
                    // the last one, is real for every sequence.
                    if ki == q_abs {
                        continue;
                    }
                    let blocked = ki < pad          // never attend to left padding
                        || ki > q_abs; // never attend to the future
                    if blocked {
                        let idx =
                            (bi * (q_len as usize) + qi as usize) * (k_len as usize) + ki as usize;
                        data[idx] = neg;
                    }
                }
            }
        }
        let mask = Array::from_slice(&data, &[b as i32, 1, q_len, k_len]);
        Ok(mask.as_dtype(dtype)?)
    }

    /// Batched autoregressive generation over a left-padded prompt batch.
    fn generate_batch(
        &mut self,
        inputs_embeds: &Array,
        pad_lens: &[i32],
        prompt_len: i32,
        config: &SamplingConfig,
    ) -> Result<Vec<String>> {
        let b = pad_lens.len();
        let hidden = self.config.text_config.hidden_size;
        let mut cache: Vec<Option<KVCache>> = Vec::new();
        let mut tokens: Vec<Vec<i32>> = vec![Vec::new(); b];
        let mut finished = vec![false; b];
        let mut recent: Vec<std::collections::VecDeque<i32>> =
            vec![std::collections::VecDeque::with_capacity(11); b];

        // Prefill: causal within the real tokens, blind to the pad region.
        let mask = Self::padding_mask(pad_lens, prompt_len, prompt_len, 0, inputs_embeds.dtype())?;
        let hidden_states =
            self.model
                .forward_embeddings_masked(inputs_embeds, &mut cache, Some(&mask))?;

        let last_hidden = hidden_states
            .index((.., -1, ..))
            .reshape(&[b as i32, 1, hidden])?;
        let logits = self.model.compute_logits(&last_hidden)?;
        let mut next = Self::sample(&logits.index((.., -1, ..)), config)?;
        eval([&next])?;
        let mut current: Vec<i32> = Self::token_column(&next, b)?;

        for _ in 0..config.max_tokens {
            for i in 0..b {
                if finished[i] {
                    continue;
                }
                let tok = current[i];
                if self.eos_token_ids.contains(&tok) {
                    finished[i] = true;
                    continue;
                }
                // Same degenerate-repetition guard as the single-sequence path.
                recent[i].push_back(tok);
                if recent[i].len() > 10 {
                    recent[i].pop_front();
                }
                if recent[i].len() >= 10 && recent[i].iter().all(|&t| t == tok) {
                    finished[i] = true;
                    continue;
                }
                tokens[i].push(tok);
            }
            if finished.iter().all(|&f| f) {
                break;
            }

            // Finished rows keep stepping with a harmless token; their output is
            // discarded, which costs nothing extra because the batch steps anyway.
            let feed: Vec<i32> = (0..b)
                .map(|i| if finished[i] { 0 } else { current[i] })
                .collect();
            let token_array = Array::from_slice(&feed, &[b as i32, 1]);
            let h = self.model.get_token_embeddings(&token_array)?;

            let k_len = prompt_len + tokens.iter().map(|t| t.len()).max().unwrap_or(0) as i32;
            let step_mask = Self::padding_mask(pad_lens, 1, k_len, k_len - 1, h.dtype())?;
            let hidden_states =
                self.model
                    .forward_embeddings_masked(&h, &mut cache, Some(&step_mask))?;

            let last_hidden = hidden_states
                .index((.., -1, ..))
                .reshape(&[b as i32, 1, hidden])?;
            let logits = self.model.compute_logits(&last_hidden)?;
            next = Self::sample(&logits.index((.., -1, ..)), config)?;
            eval([&next])?;
            current = Self::token_column(&next, b)?;
        }

        tokens.iter().map(|t| self.decode_tokens(t)).collect()
    }

    /// Read a sampled `[batch]` (or `[batch, 1]`) token array back to host ints.
    fn token_column(sampled: &Array, b: usize) -> Result<Vec<i32>> {
        // argmax yields an unsigned index and `categorical` an integer type, so
        // normalise before reading rather than assuming either.
        let flat = sampled.reshape(&[-1])?.as_dtype(mlx_rs::Dtype::Int32)?;
        let slice: &[i32] = flat
            .try_as_slice::<i32>()
            .map_err(|e| Error::Inference(format!("Failed to read sampled tokens: {}", e)))?;
        if slice.len() != b {
            return Err(Error::Inference(format!(
                "Sampler returned {} tokens for a batch of {}",
                slice.len(),
                b
            )));
        }
        Ok(slice.to_vec())
    }

    /// Build prompt token IDs.
    fn build_prompt(&self, num_audio_tokens: i32, language: Option<&str>) -> Result<Array> {
        let tokenizer = self
            .tokenizer
            .as_ref()
            .ok_or_else(|| Error::Tokenizer("Tokenizer not loaded".to_string()))?;

        let assistant_prefix = language.map_or_else(String::new, |language| {
            format!("language {language}<asr_text>")
        });
        let prompt = format!(
            "<|im_start|>system\n<|im_end|>\n<|im_start|>user\n<|audio_start|>{}<|audio_end|><|im_end|>\n<|im_start|>assistant\n{}",
            "<|audio_pad|>".repeat(num_audio_tokens as usize),
            assistant_prefix,
        );

        let encoding = tokenizer
            .encode(prompt.as_str(), false)
            .map_err(|e| Error::Tokenizer(e.to_string()))?;

        let ids: Vec<i32> = encoding.get_ids().iter().map(|&id| id as i32).collect();
        let len = ids.len() as i32;
        Ok(Array::from_slice(&ids, &[1, len]))
    }

    /// Build input embeddings with audio features replacing audio_pad tokens.
    fn build_inputs_embeds(&mut self, input_ids: &Array, audio_features: &Array) -> Result<Array> {
        // Find audio_pad token positions first (input_ids is already concrete from from_slice)
        let audio_token_id = self.config.audio_token_id;

        let ids_flat = input_ids.reshape(&[-1])?;
        // input_ids was built from Array::from_slice — already materialized, no eval needed
        let ids_data: &[i32] = ids_flat
            .try_as_slice::<i32>()
            .map_err(|e| Error::Inference(format!("Failed to read input_ids: {}", e)))?;

        let mut first_audio = None;
        let mut last_audio = None;
        for (i, &id) in ids_data.iter().enumerate() {
            if id == audio_token_id {
                if first_audio.is_none() {
                    first_audio = Some(i);
                }
                last_audio = Some(i);
            }
        }

        // Get text embeddings (lazy — eval deferred to concatenation)
        let embeddings = self.model.get_token_embeddings(input_ids)?;

        if let (Some(first), Some(last)) = (first_audio, last_audio) {
            let audio_start = first as i32;
            let audio_end = (last + 1) as i32;

            let prefix_embed = embeddings.index((.., ..audio_start, ..));
            let suffix_embed = embeddings.index((.., audio_end.., ..));

            let audio_embed = audio_features.reshape(&[
                1,
                audio_features.shape()[0],
                audio_features.shape()[1],
            ])?;

            let audio_embed = audio_embed.as_dtype(embeddings.dtype())?;

            let result =
                mlx_rs::ops::concatenate_axis(&[&prefix_embed, &audio_embed, &suffix_embed], 1)?;
            Ok(result)
        } else {
            Ok(embeddings)
        }
    }

    /// Autoregressive text generation.
    fn generate(
        &mut self,
        inputs_embeds: &Array,
        config: &SamplingConfig,
    ) -> Result<(String, u32)> {
        let mut cache: Vec<Option<KVCache>> = Vec::new();
        let mut tokens: Vec<i32> = Vec::new();
        let eos_tokens = &self.eos_token_ids;

        // First forward pass with full prompt
        let hidden_states = self.model.forward_embeddings(inputs_embeds, &mut cache)?;

        // Get logits from last position using tied weights
        let last_hidden = hidden_states.index((.., -1, ..));
        let last_hidden = last_hidden.reshape(&[1, 1, self.config.text_config.hidden_size])?;
        let logits = self.model.compute_logits(&last_hidden)?;

        let last_logits = logits.index((.., -1, ..));
        let token = Self::sample_with_repetition_penalty(&last_logits, config, &tokens)?;
        eval([&token])?;
        let mut token_id = token.item::<i32>();

        let mut recent_tokens: std::collections::VecDeque<i32> =
            std::collections::VecDeque::with_capacity(11);

        for _ in 0..config.max_tokens {
            if eos_tokens.contains(&token_id) {
                break;
            }

            // Repetition detection
            recent_tokens.push_back(token_id);
            if recent_tokens.len() > 10 {
                recent_tokens.pop_front();
            }
            if recent_tokens.len() >= 10 && recent_tokens.iter().all(|&t| t == token_id) {
                break;
            }

            tokens.push(token_id);

            // Get embedding for next token
            let token_array = Array::from_slice(&[token_id], &[1, 1]);
            let h = self.model.get_token_embeddings(&token_array)?;

            // Forward through decoder
            let hidden_states = self.model.forward_embeddings(&h, &mut cache)?;
            let last_hidden = hidden_states.index((.., -1, ..));
            let last_hidden = last_hidden.reshape(&[1, 1, self.config.text_config.hidden_size])?;
            let logits = self.model.compute_logits(&last_hidden)?;

            let last_logits = logits.index((.., -1, ..));
            let token = Self::sample_with_repetition_penalty(&last_logits, config, &tokens)?;
            eval([&token])?;
            token_id = token.item::<i32>();
        }

        // Decode tokens
        Ok((self.decode_tokens(&tokens)?, tokens.len() as u32))
    }

    /// Sample from logits.
    fn sample(
        logits: &Array,
        config: &SamplingConfig,
    ) -> std::result::Result<Array, mlx_rs::error::Exception> {
        if config.temperature == 0.0 {
            argmax_axis(logits, -1, false)
        } else {
            let scaled = logits.multiply(&Array::from(1.0 / config.temperature))?;
            mlx_rs::random::categorical(&scaled, None, None, None)
        }
    }

    fn sample_with_repetition_penalty(
        logits: &Array,
        config: &SamplingConfig,
        generated_tokens: &[i32],
    ) -> Result<Array> {
        let Some(penalty) = config.repetition_penalty else {
            return Self::sample(logits, config).map_err(Error::from);
        };
        if !penalty.is_finite() || penalty <= 0.0 {
            return Err(Error::Config(
                "repetition_penalty must be a finite value greater than zero".into(),
            ));
        }
        if generated_tokens.is_empty() || penalty == 1.0 {
            return Self::sample(logits, config).map_err(Error::from);
        }

        let mut unique_tokens = Vec::with_capacity(generated_tokens.len());
        for &token in generated_tokens {
            if !unique_tokens.contains(&token) {
                unique_tokens.push(token);
            }
        }
        let vocab_size = *logits
            .shape()
            .last()
            .ok_or_else(|| Error::Inference("Sampler received scalar logits".into()))?;
        if unique_tokens
            .iter()
            .any(|&token| token < 0 || token >= vocab_size)
        {
            return Err(Error::Inference(
                "Generated token ID is outside the logits vocabulary".into(),
            ));
        }

        let index_shape = [1, unique_tokens.len() as i32];
        let indices = Array::from_slice(&unique_tokens, &index_shape);
        let selected = mlx_rs::ops::indexing::take_along_axis(logits, &indices, -1)?;
        let zero = Array::from(0.0f32).as_dtype(logits.dtype())?;
        let penalty_array = Array::from(penalty).as_dtype(logits.dtype())?;
        let positive_logits = selected.divide(&penalty_array)?;
        let negative_logits = selected.multiply(&penalty_array)?;
        let penalized =
            mlx_rs::ops::r#where(&selected.ge(&zero)?, &positive_logits, &negative_logits)?;
        let adjusted = mlx_rs::ops::indexing::put_along_axis(logits, &indices, &penalized, -1)?;
        Self::sample(&adjusted, config).map_err(Error::from)
    }

    /// Decode token IDs to text.
    fn decode_tokens(&self, tokens: &[i32]) -> Result<String> {
        if let Some(ref tokenizer) = self.tokenizer {
            let token_ids: Vec<u32> = tokens.iter().map(|&t| t as u32).collect();
            tokenizer
                .decode(&token_ids, true)
                .map_err(|e| Error::Tokenizer(e.to_string()))
        } else {
            Ok(tokens
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Qwen3ASR;
    use crate::error::Error;

    #[test]
    fn auto_output_keeps_plain_text_without_claiming_a_detected_language() {
        assert_eq!(
            Qwen3ASR::parse_auto_output("直接の文字起こし"),
            ("Unknown", "直接の文字起こし")
        );
        assert_eq!(Qwen3ASR::parse_auto_output(" "), ("Unknown", ""));
    }

    #[test]
    fn auto_output_treats_language_none_as_no_detected_speech() {
        assert_eq!(
            Qwen3ASR::parse_auto_output("language None<asr_text>"),
            ("Unknown", "")
        );
    }

    #[test]
    fn auto_output_parses_detected_language_with_metadata_lines() {
        assert_eq!(
            Qwen3ASR::parse_auto_output("language Japanese\n<asr_text>こんにちは"),
            ("Japanese", "こんにちは")
        );
        assert_eq!(
            Qwen3ASR::parse_auto_output("language\tJapanese<asr_text>こんにちは"),
            ("Japanese", "こんにちは")
        );
        assert_eq!(
            Qwen3ASR::parse_auto_output("language\nJapanese<asr_text>こんにちは"),
            ("Japanese", "こんにちは")
        );
    }

    #[test]
    fn generation_eos_ids_are_preserved_and_deduplicated() {
        let config = serde_json::json!({ "eos_token_id": [151643, 151645, 151643] });
        assert_eq!(
            Qwen3ASR::parse_generation_eos_tokens(&config, 151645).unwrap(),
            vec![151643, 151645]
        );

        let scalar_config = serde_json::json!({ "eos_token_id": 151645 });
        assert_eq!(
            Qwen3ASR::parse_generation_eos_tokens(&scalar_config, 151645).unwrap(),
            vec![151645]
        );
    }

    #[test]
    fn generation_eos_ids_require_tokenizer_eos() {
        let config = serde_json::json!({ "eos_token_id": [151643] });
        assert!(matches!(
            Qwen3ASR::parse_generation_eos_tokens(&config, 151645),
            Err(Error::Tokenizer(message)) if message.contains("does not include tokenizer EOS ID")
        ));
    }

    #[test]
    fn repetition_penalty_is_applied_before_greedy_sampling() {
        mlx_rs::with_new_default_stream(mlx_rs::Stream::cpu(), || {
            let logits = mlx_rs::Array::from_slice(&[1.0f32, 0.9], &[1, 2]);
            let config = super::SamplingConfig {
                temperature: 0.0,
                max_tokens: 8,
                repetition_penalty: Some(2.0),
            };
            let sampled = Qwen3ASR::sample_with_repetition_penalty(&logits, &config, &[0]).unwrap();
            super::eval([&sampled]).unwrap();
            assert_eq!(sampled.item::<i32>(), 1);
        });
    }
}
