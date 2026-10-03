use std::{env, process::Command, time::Duration};

use reco_gui_lib::application_core::{
    domain::{SplitReason, VadDiagnostics},
    worker::{AsrEngine, SegmentTranscribeRequest, WorkerTranscriptionConfig},
};

fn child_processes() -> Vec<String> {
    let output = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires external llama-server, a cached Qwen3-ASR GGUF and a Japanese 16 kHz WAV"]
async fn cached_qwen3_model_transcribes_with_forced_and_detected_language() {
    let repo_id = env::var("RECOGUI_TEST_MODEL_REPO").unwrap();
    let revision = env::var("RECOGUI_TEST_MODEL_REVISION").unwrap();
    let file_name = env::var("RECOGUI_TEST_MODEL_FILE").unwrap();
    let audio_path = env::var("RECOGUI_TEST_AUDIO").unwrap();
    let mut audio = hound::WavReader::open(audio_path).unwrap();
    assert_eq!(audio.spec().sample_rate, 16_000);
    assert_eq!(audio.spec().channels, 1);
    let samples: Vec<f32> = audio
        .samples::<i16>()
        .map(|value| value.unwrap() as f32 / 32768.0)
        .collect();
    let initial_children = child_processes();

    let worker = AsrEngine::launch().await.unwrap();
    let models = worker.list_models().await.unwrap().models;
    assert!(models.iter().any(|model| model.repo_id == repo_id
        && model.revision == revision
        && model.file_name == file_name));
    worker
        .load_model(&repo_id, &revision, &file_name)
        .await
        .unwrap();
    assert!(
        worker
            .load_model(&repo_id, &revision, &file_name)
            .await
            .unwrap()
            .reused
    );
    let request = |samples: &[f32], language, repetition_penalty| SegmentTranscribeRequest {
        session_id: "session".into(),
        run_id: "run".into(),
        job_id: "job".into(),
        segment_index: 0,
        start_sample: 0,
        end_sample: samples.len() as u64,
        sample_rate: 16_000,
        split_reason: SplitReason::EndOfInput,
        language,
        vad: VadDiagnostics {
            mean_probability: 0.5,
            peak_probability: 0.5,
            speech_ratio: 0.5,
        },
        options: WorkerTranscriptionConfig {
            generation_tokens_per_second: 20.0,
            max_generation_tokens: 2048,
            min_generation_tokens: 64,
            temperature: 0.0,
            repetition_penalty,
        },
    };
    for (language, repetition_penalty) in [
        (Some("Japanese".into()), None),
        (None, None),
        (Some("Japanese".into()), Some(1.1)),
    ] {
        let result = worker
            .transcribe_segment(&request(&samples, language, repetition_penalty), &samples)
            .await
            .unwrap();
        eprintln!(
            "{}: {} ({} ms)",
            result.language,
            result.text,
            result.diagnostics.model_total_time_ms.unwrap()
        );
        assert!(result.text.contains("テスト"));
        assert_eq!(result.language, "Japanese");
        assert!(result.diagnostics.generation_tokens.unwrap() > 0);
        assert!(!result.text.contains("<asr_text>"));
    }
    let long_samples = samples.repeat(9);
    assert!(long_samples.len() > 30 * 16_000);
    let result = worker
        .transcribe_segment(
            &request(&long_samples, Some("Japanese".into()), None),
            &long_samples,
        )
        .await
        .unwrap();
    assert!(result.text.contains("テスト"));
    assert!(worker.unload_model().await.unwrap().unloaded);
    assert_eq!(child_processes(), initial_children);
    worker
        .load_model(&repo_id, &revision, &file_name)
        .await
        .unwrap();
    worker.shutdown().await.unwrap();
    assert_eq!(child_processes(), initial_children);

    let worker = AsrEngine::launch().await.unwrap();
    worker
        .load_model(&repo_id, &revision, &file_name)
        .await
        .unwrap();
    drop(worker);
    tokio::time::timeout(Duration::from_secs(5), async {
        while child_processes() != initial_children {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropping the engine must reap its llama-server process");
}
