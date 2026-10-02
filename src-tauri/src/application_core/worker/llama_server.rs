use std::{
    env,
    io::{Cursor, Read, Seek, SeekFrom},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::json;
use tempfile::NamedTempFile;

use super::{model_catalog::ModelFiles, types::WorkerTranscriptionConfig};
use crate::application_core::error::CoreError;

#[derive(Default)]
pub(super) struct ProcessState {
    pub child: Option<Child>,
    pub closed: bool,
}

pub(super) type ProcessSlot = Arc<Mutex<ProcessState>>;

pub(super) struct TranscriptionOutput {
    pub text: String,
    pub language: String,
    pub generation_tokens: u32,
    pub prompt_tokens: u32,
    pub token_limit_reached: bool,
}

pub(super) struct LlamaServer {
    process: ProcessSlot,
    client: Client,
    url: String,
    key: String,
    marker: String,
    log: NamedTempFile,
}

#[derive(Deserialize)]
struct Completion {
    content: String,
    tokens_predicted: u32,
    tokens_evaluated: u32,
    stop_type: String,
    truncated: bool,
}

impl LlamaServer {
    pub fn load(files: &ModelFiles, process: ProcessSlot) -> Result<Self, CoreError> {
        Self::start(files, process, Command::new(executable()?))
    }

    pub(super) fn start(
        files: &ModelFiles,
        process: ProcessSlot,
        mut command: Command,
    ) -> Result<Self, CoreError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let key = uuid::Uuid::new_v4().to_string();
        let log = NamedTempFile::new()?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(transport_error)?;
        drop(listener);
        let mut child = command
            .arg("--model")
            .arg(&files.model)
            .arg("--mmproj")
            .arg(&files.projector)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .args(["--ctx-size", "8192", "--parallel", "1"])
            .args(["--offline", "--no-webui", "--no-warmup"])
            .arg("--api-key")
            .arg(&key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log.reopen()?))
            .spawn()
            .map_err(|error| {
                CoreError::WorkerUnavailable(format!(
                    "could not start {}: {error}",
                    command.get_program().to_string_lossy()
                ))
            })?;
        {
            let mut state = process.lock().expect("ASR process lock");
            if state.closed {
                stop_child(&mut child)?;
                return Err(CoreError::WorkerClosed);
            }
            state.child = Some(child);
        }
        let mut server = Self {
            process,
            client,
            url: format!("http://127.0.0.1:{port}"),
            key,
            marker: String::new(),
            log,
        };
        server.wait_ready()?;
        let properties: serde_json::Value = server
            .client
            .get(format!("{}/props", server.url))
            .bearer_auth(&server.key)
            .send()
            .map_err(transport_error)?
            .error_for_status()
            .map_err(transport_error)?
            .json()
            .map_err(transport_error)?;
        if properties
            .pointer("/modalities/audio")
            .and_then(|value| value.as_bool())
            != Some(true)
        {
            return Err(CoreError::WorkerUnavailable(
                "llama-server did not load an audio-capable model and projector".into(),
            ));
        }
        server.marker = properties.get("media_marker").and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CoreError::WorkerUnavailable("llama-server must support audio input and /props media_marker; install a current llama.cpp release".into()))?
            .to_owned();
        Ok(server)
    }

    fn wait_ready(&mut self) -> Result<(), CoreError> {
        let started = Instant::now();
        loop {
            self.ensure_running()?;
            match self
                .client
                .get(format!("{}/health", self.url))
                .timeout(Duration::from_secs(1))
                .send()
            {
                Ok(response) if response.status().is_success() => {
                    let health: serde_json::Value = response.json().map_err(transport_error)?;
                    if health.get("status").and_then(|value| value.as_str()) != Some("ok") {
                        return Err(CoreError::WorkerProtocol(
                            "llama-server returned an invalid health response".into(),
                        ));
                    }
                    return Ok(());
                }
                Ok(response) if response.status().as_u16() == 503 => {}
                Ok(response) => {
                    return Err(CoreError::WorkerUnavailable(format!(
                        "llama-server readiness returned HTTP {}",
                        response.status()
                    )));
                }
                Err(error) if error.is_connect() || error.is_timeout() => {}
                Err(error) => return Err(transport_error(error)),
            }
            if started.elapsed() >= Duration::from_secs(180) {
                return Err(CoreError::WorkerUnavailable(format!(
                    "llama-server did not become ready within 180 seconds: {}",
                    self.log_tail()?
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn ensure_running(&mut self) -> Result<(), CoreError> {
        let status = {
            let mut process = self.process.lock().expect("ASR process lock");
            process
                .child
                .as_mut()
                .ok_or_else(|| CoreError::WorkerExited("llama-server was stopped".into()))?
                .try_wait()?
        };
        if let Some(status) = status {
            return Err(CoreError::WorkerExited(format!(
                "llama-server exited ({status}): {}",
                self.log_tail()?
            )));
        }
        Ok(())
    }

    fn log_tail(&mut self) -> Result<String, CoreError> {
        let file = self.log.as_file_mut();
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn generate(
        &mut self,
        samples: &[f32],
        language: Option<&str>,
        options: &WorkerTranscriptionConfig,
        max_tokens: u32,
    ) -> Result<TranscriptionOutput, CoreError> {
        self.ensure_running()?;
        let prefix = language
            .map(|value| format!("language {value}<asr_text>"))
            .unwrap_or_default();
        let prompt = format!(
            "<|im_start|>system\n<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n{prefix}",
            self.marker
        );
        let body = json!({
            "prompt": { "prompt_string": prompt, "multimodal_data": [STANDARD.encode(wav(samples)?)] },
            "n_predict": max_tokens,
            "temperature": options.temperature,
            "repeat_penalty": options.repetition_penalty.unwrap_or(1.0),
            "stream": false,
            "cache_prompt": false,
        });
        let response = self
            .client
            .post(format!("{}/completion", self.url))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .map_err(transport_error)?;
        if !response.status().is_success() {
            let status = response.status();
            let message = response.text().map_err(transport_error)?;
            return Err(CoreError::WorkerResponse {
                code: "transcriptionFailure".into(),
                message: format!("llama-server HTTP {status}: {message}"),
                recoverable: status.is_client_error(),
            });
        }
        let output: Completion = response.json().map_err(|error| {
            CoreError::WorkerProtocol(format!("invalid llama-server completion response: {error}"))
        })?;
        if output.truncated {
            return Err(CoreError::WorkerResponse {
                code: "transcriptionFailure".into(),
                message: "llama-server truncated the audio context".into(),
                recoverable: true,
            });
        }
        if !matches!(output.stop_type.as_str(), "eos" | "word" | "limit") {
            return Err(CoreError::WorkerProtocol(format!(
                "unexpected llama-server stop_type: {}",
                output.stop_type
            )));
        }
        let (text, detected_language) = parse_output(&output.content, language);
        Ok(TranscriptionOutput {
            text,
            language: detected_language,
            generation_tokens: output.tokens_predicted,
            prompt_tokens: output.tokens_evaluated,
            token_limit_reached: output.stop_type == "limit",
        })
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        let _ = stop_process(&self.process);
    }
}

pub(super) fn stop_process(process: &ProcessSlot) -> Result<(), CoreError> {
    let mut process = process.lock().expect("ASR process lock");
    if let Some(mut child) = process.child.take() {
        stop_child(&mut child)?;
    }
    Ok(())
}

fn stop_child(child: &mut Child) -> Result<(), CoreError> {
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    child.wait()?;
    Ok(())
}

fn executable() -> Result<PathBuf, CoreError> {
    if let Some(path) = env::var_os("RECOGUI_LLAMA_SERVER") {
        if path.is_empty() {
            return Err(CoreError::WorkerUnavailable(
                "RECOGUI_LLAMA_SERVER is empty".into(),
            ));
        }
        return Ok(path.into());
    }
    let mut directories: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    directories.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/run/current-system/sw/bin",
            "/nix/var/nix/profiles/default/bin",
        ]
        .map(PathBuf::from),
    );
    if let Some(home) = env::var_os("HOME") {
        directories.push(PathBuf::from(home).join(".nix-profile/bin"));
    }
    directories.into_iter().map(|directory| directory.join("llama-server")).find(|path| path.is_file())
        .ok_or_else(|| CoreError::WorkerUnavailable("llama-server is required; install llama.cpp or set RECOGUI_LLAMA_SERVER to its executable".into()))
}

fn transport_error(error: reqwest::Error) -> CoreError {
    CoreError::WorkerUnavailable(format!("llama-server request failed: {error}"))
}

fn wav(samples: &[f32]) -> Result<Vec<u8>, CoreError> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|error| CoreError::AudioDecode(error.to_string()))?;
        for sample in samples {
            writer
                .write_sample(*sample)
                .map_err(|error| CoreError::AudioDecode(error.to_string()))?;
        }
        writer
            .finalize()
            .map_err(|error| CoreError::AudioDecode(error.to_string()))?;
    }
    Ok(cursor.into_inner())
}

fn parse_output(raw: &str, forced_language: Option<&str>) -> (String, String) {
    let raw = raw.replace("<|im_end|>", "");
    if let Some(language) = forced_language {
        return (
            raw.split_once("<asr_text>")
                .map_or(raw.as_str(), |(_, text)| text)
                .to_owned(),
            language.to_owned(),
        );
    }
    if let Some((header, text)) = raw.split_once("<asr_text>") {
        let language = header
            .trim()
            .strip_prefix("language")
            .filter(|value| value.starts_with(char::is_whitespace))
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "None")
            .unwrap_or("Unknown");
        (text.to_owned(), language.to_owned())
    } else {
        (raw, "Unknown".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(scenario: &str) -> (tempfile::TempDir, ModelFiles, ProcessSlot, Command) {
        let directory = tempfile::tempdir().unwrap();
        let files = ModelFiles {
            model: directory.path().join("model.gguf"),
            projector: directory.path().join("mmproj.gguf"),
        };
        let process = Arc::new(Mutex::new(ProcessState::default()));
        let mut command = Command::new("node");
        command
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../fixtures/native/llamaServer.mjs"
            ))
            .env("RECOGUI_FIXTURE_SCENARIO", scenario)
            .env(
                "RECOGUI_FIXTURE_LOG",
                directory.path().join("requests.jsonl"),
            );
        (directory, files, process, command)
    }

    fn options() -> WorkerTranscriptionConfig {
        WorkerTranscriptionConfig {
            generation_tokens_per_second: 20.0,
            max_generation_tokens: 2048,
            min_generation_tokens: 64,
            temperature: 0.0,
            repetition_penalty: Some(1.1),
        }
    }

    #[test]
    fn external_server_receives_in_memory_wav_and_is_reaped() {
        let (directory, files, process, command) = fixture("normal");
        let mut server = LlamaServer::start(&files, process.clone(), command).unwrap();
        let samples = [0.25, -0.5, 0.0];
        let result = server.generate(&samples, None, &options(), 64).unwrap();
        assert_eq!(result.text, "これはテストです。");
        assert_eq!(result.language, "Japanese");
        assert_eq!(result.generation_tokens, 8);
        assert_eq!(result.prompt_tokens, 100);
        let log = std::fs::read_to_string(directory.path().join("requests.jsonl")).unwrap();
        let entries: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries[0]["model"], files.model.to_str().unwrap());
        assert_eq!(entries[0]["projector"], files.projector.to_str().unwrap());
        let request = &entries[1]["value"];
        assert!(
            request["prompt"]["prompt_string"]
                .as_str()
                .unwrap()
                .contains("<__media_fixture__>")
        );
        assert_eq!(request["n_predict"], 64);
        assert!((request["repeat_penalty"].as_f64().unwrap() - 1.1).abs() < 1e-6);
        let encoded = request["prompt"]["multimodal_data"][0].as_str().unwrap();
        let mut wav =
            hound::WavReader::new(Cursor::new(STANDARD.decode(encoded).unwrap())).unwrap();
        assert_eq!(wav.spec().sample_rate, 16_000);
        assert_eq!(wav.spec().channels, 1);
        assert_eq!(
            wav.samples::<f32>().map(Result::unwrap).collect::<Vec<_>>(),
            samples
        );
        let pid = entries[0]["pid"].as_u64().unwrap();
        drop(server);
        assert!(process.lock().unwrap().child.is_none());
        assert!(
            !Command::new("ps")
                .args(["-p", &pid.to_string()])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn external_server_failures_are_visible_and_cleanup_the_child() {
        for scenario in ["earlyExit", "http500", "malformed"] {
            let (_directory, files, process, command) = fixture(scenario);
            match LlamaServer::start(&files, process.clone(), command) {
                Ok(mut server) => {
                    assert!(
                        server.generate(&[0.0; 1600], None, &options(), 64).is_err(),
                        "{scenario}"
                    );
                    drop(server);
                }
                Err(error) => {
                    assert_eq!(scenario, "earlyExit");
                    assert!(
                        error
                            .to_string()
                            .contains("fixture model could not be loaded")
                    );
                }
            }
            assert!(process.lock().unwrap().child.is_none());
        }
    }

    #[test]
    fn missing_external_runtime_is_an_explicit_error() {
        let (directory, files, process, _) = fixture("normal");
        let command = Command::new(directory.path().join("missing-llama-server"));
        assert!(matches!(
            LlamaServer::start(&files, process.clone(), command),
            Err(CoreError::WorkerUnavailable(message)) if message.contains("could not start")
        ));
        assert!(process.lock().unwrap().child.is_none());
    }

    #[test]
    fn closing_during_model_startup_reaps_the_child() {
        let (_directory, files, process, command) = fixture("loading");
        let loader_process = process.clone();
        let loader = thread::spawn(move || LlamaServer::start(&files, loader_process, command));
        let started = Instant::now();
        while process.lock().unwrap().child.is_none() {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(10));
        }
        process.lock().unwrap().closed = true;
        stop_process(&process).unwrap();
        assert!(loader.join().unwrap().is_err());
        assert!(process.lock().unwrap().child.is_none());
    }

    #[test]
    fn shutdown_reaps_a_child_that_already_exited() {
        let mut child = Command::new("node")
            .args(["-e", "process.exit(23)"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap();
        stop_child(&mut child).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(23));
        assert!(
            !Command::new("ps")
                .args(["-p", &pid.to_string()])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn closing_before_model_startup_does_not_retain_a_child() {
        let (_directory, files, process, command) = fixture("earlyExit");
        process.lock().unwrap().closed = true;
        assert!(matches!(
            LlamaServer::start(&files, process.clone(), command),
            Err(CoreError::WorkerClosed)
        ));
        assert!(process.lock().unwrap().child.is_none());
    }

    #[test]
    fn language_tag_absence_keeps_the_transcript() {
        for (raw, text, language) in [
            (
                "language Japanese<asr_text>こんにちは",
                "こんにちは",
                "Japanese",
            ),
            (
                "language\tJapanese<asr_text>こんにちは",
                "こんにちは",
                "Japanese",
            ),
            ("language None<asr_text>こんにちは", "こんにちは", "Unknown"),
            ("こんにちは", "こんにちは", "Unknown"),
            ("", "", "Unknown"),
        ] {
            assert_eq!(parse_output(raw, None), (text.into(), language.into()));
        }
        assert_eq!(
            parse_output("こんにちは", Some("Japanese")),
            ("こんにちは".into(), "Japanese".into())
        );
    }
}
