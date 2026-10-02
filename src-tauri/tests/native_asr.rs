use std::{env, time::Duration};

use qwen3_asr_mlx::{audio::load_wav, memory::memory_snapshot};
use reco_gui_lib::application_core::{
    domain::{SplitReason, VadDiagnostics},
    worker::{AsrEngine, SegmentTranscribeRequest, WorkerTranscriptionConfig},
};

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a local Qwen3-ASR cache revision and Japanese 16 kHz WAV"]
async fn cached_qwen3_model_transcribes_with_forced_and_detected_language() {
    let repo_id = env::var("RECOGUI_TEST_MODEL_REPO").unwrap();
    let revision = env::var("RECOGUI_TEST_MODEL_REVISION").unwrap();
    let audio_path = env::var("RECOGUI_TEST_AUDIO").unwrap();
    let (samples, sample_rate) = load_wav(audio_path).unwrap();
    assert_eq!(sample_rate, 16_000);
    let initial_memory = memory_snapshot();

    let worker = AsrEngine::launch().await.unwrap();
    let models = worker.list_models().await.unwrap().models;
    assert!(
        models
            .iter()
            .any(|model| model.repo_id == repo_id && model.revision == revision)
    );
    worker
        .load_model(repo_id.clone(), revision.clone())
        .await
        .unwrap();
    eprintln!("after model load: {}", memory_snapshot());

    for (language, repetition_penalty) in [
        (Some("Japanese"), None),
        (None, None),
        (Some("Japanese"), Some(1.1)),
    ] {
        let request = SegmentTranscribeRequest {
            session_id: "session".into(),
            run_id: "run".into(),
            job_id: "job".into(),
            segment_index: 0,
            start_sample: 0,
            end_sample: samples.len() as u64,
            sample_rate,
            split_reason: SplitReason::EndOfInput,
            language: language.map(str::to_owned),
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
        let result = worker.transcribe_segment(&request, &samples).await.unwrap();
        eprintln!(
            "after transcription ({} ms): {}",
            result.diagnostics.model_total_time_ms.unwrap(),
            memory_snapshot()
        );
        eprintln!(
            "language={language:?}, repetition_penalty={repetition_penalty:?}: {}",
            result.text
        );
        assert!(!result.text.is_empty());
        assert!(result.text.contains("テスト"));
        assert_eq!(result.language, "Japanese");
        assert!(result.diagnostics.generation_tokens.is_some());
        assert!(memory_snapshot().cache < 1024 * 1024 * 1024);
    }

    let long_samples = samples.repeat(9);
    let request = SegmentTranscribeRequest {
        session_id: "session".into(),
        run_id: "run".into(),
        job_id: "long-job".into(),
        segment_index: 1,
        start_sample: 0,
        end_sample: long_samples.len() as u64,
        sample_rate,
        split_reason: SplitReason::AdaptiveSplit,
        language: Some("Japanese".into()),
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
            repetition_penalty: None,
        },
    };
    let long_result = worker
        .transcribe_segment(&request, &long_samples)
        .await
        .unwrap();
    assert!(!long_result.text.is_empty());
    let nominal_tokens = (long_samples.len() as f32 / sample_rate as f32 * 20.0) as u32;
    assert!(long_result.diagnostics.max_tokens <= nominal_tokens + 64);
    eprintln!(
        "after long transcription ({} ms): {}",
        long_result.diagnostics.model_total_time_ms.unwrap(),
        memory_snapshot()
    );
    assert!(memory_snapshot().cache < 1024 * 1024 * 1024);

    assert!(worker.unload_model().await.unwrap().unloaded);
    let unloaded_memory = memory_snapshot();
    eprintln!("after model unload: {unloaded_memory}");
    assert_eq!(
        unloaded_memory.cache, 0,
        "unused GPU buffers must be released"
    );
    assert!(
        // MLX retains small runtime buffers after the model is dropped.
        unloaded_memory.active <= initial_memory.active + 64 * 1024,
        "model GPU buffers must be released"
    );
    worker
        .load_model(repo_id.clone(), revision.clone())
        .await
        .unwrap();
    worker.shutdown().await.unwrap();
    let shutdown_memory = memory_snapshot();
    assert_eq!(shutdown_memory.cache, 0);
    assert!(shutdown_memory.active <= initial_memory.active + 64 * 1024);

    let worker = AsrEngine::launch().await.unwrap();
    worker.load_model(repo_id, revision).await.unwrap();
    drop(worker);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let memory = memory_snapshot();
            if memory.cache == 0 && memory.active <= initial_memory.active + 64 * 1024 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropping the engine must release its GPU memory");
}
