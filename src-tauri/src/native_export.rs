//! Pure Rust transcript rendering and atomic export.
//!
//! This module deliberately has no Tauri or database dependencies.  The application core takes a
//! read-only snapshot and hands it to these functions.  A snapshot is rendered completely before
//! the destination is published, so a cancelled or failed export cannot leave a partial result.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::api_types::ExportFormat;

impl ExportFormat {
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Txt | Self::TimestampedTxt => "txt",
            Self::Markdown => "md",
            Self::Json => "json",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::TimestampedTxt => "timestampedTxt",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
        }
    }
}

/// The data needed to render one transcript session.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSession {
    pub session_id: String,
    pub title: String,
    pub sample_rate: u32,
    pub segments: Vec<ExportSegment>,
}

/// A transcript segment expressed in the normalized sample clock.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSegment {
    #[serde(rename = "segmentIndex", alias = "index")]
    pub segment_index: u32,
    pub start_sample: u64,
    pub end_sample: u64,
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub split_reason: Option<String>,
    #[serde(default)]
    pub raw_text: Option<String>,
    #[serde(default)]
    pub diagnostics: Option<serde_json::Value>,
}

/// A cooperative cancellation handle owned by the application core.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("at least one session is required")]
    EmptySelection,
    #[error("session {session_id} has an invalid sample rate")]
    InvalidSampleRate { session_id: String },
    #[error("session {session_id} has an invalid segment range")]
    InvalidSegmentRange { session_id: String },
    #[error("session {session_id} has unordered or duplicate segments")]
    UnorderedSegments { session_id: String },
    #[error("export was cancelled")]
    Cancelled,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// The result of a successfully published export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportResult {
    pub destination: PathBuf,
    pub exported_session_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportStage {
    Writing,
    Publishing,
}

/// Render one session without touching the filesystem.
pub fn render_session(
    session: &ExportSession,
    format: ExportFormat,
) -> Result<String, ExportError> {
    validate_session(session)?;
    match format {
        ExportFormat::Txt => Ok(session
            .segments
            .iter()
            .filter(|segment| !segment.text.is_empty())
            .map(|segment| normalize_newlines(&segment.text))
            .collect::<Vec<_>>()
            .join("\n")),
        ExportFormat::TimestampedTxt => Ok(session
            .segments
            .iter()
            .filter(|segment| !segment.text.is_empty())
            .map(|segment| {
                let timestamp = format_timestamp(
                    segment.start_sample,
                    session.sample_rate,
                    TimestampStyle::Vtt,
                )?;
                Ok(format!(
                    "[{}] {}",
                    timestamp,
                    normalize_newlines(&segment.text)
                ))
            })
            .collect::<Result<Vec<_>, ExportError>>()?
            .join("\n")),
        ExportFormat::Markdown => {
            let lines = session
                .segments
                .iter()
                .filter(|segment| !segment.text.is_empty())
                .map(|segment| {
                    Ok(format!(
                        "[{}] {}",
                        format_timestamp(
                            segment.start_sample,
                            session.sample_rate,
                            TimestampStyle::Vtt
                        )?,
                        normalize_newlines(&segment.text)
                    ))
                })
                .collect::<Result<Vec<_>, ExportError>>()?;
            if lines.is_empty() {
                Ok(format!("# {}", session.title))
            } else {
                Ok(format!("# {}\n{}", session.title, lines.join("  \n")))
            }
        }
        ExportFormat::Json => Ok(format_json(session)?),
        ExportFormat::Srt => {
            let mut cues = Vec::with_capacity(session.segments.len() * 4);
            for (position, segment) in session.segments.iter().enumerate() {
                cues.push((position + 1).to_string());
                cues.push(format!(
                    "{} --> {}",
                    format_timestamp(
                        segment.start_sample,
                        session.sample_rate,
                        TimestampStyle::Srt
                    )?,
                    format_timestamp(segment.end_sample, session.sample_rate, TimestampStyle::Srt)?
                ));
                cues.push(normalize_newlines(&segment.text));
                cues.push(String::new());
            }
            Ok(cues.join("\n"))
        }
        ExportFormat::Vtt => {
            let mut cues = vec!["WEBVTT".to_owned(), String::new()];
            for segment in &session.segments {
                cues.push(format!(
                    "{} --> {}",
                    format_timestamp(
                        segment.start_sample,
                        session.sample_rate,
                        TimestampStyle::Vtt
                    )?,
                    format_timestamp(segment.end_sample, session.sample_rate, TimestampStyle::Vtt)?
                ));
                cues.push(normalize_newlines(&segment.text));
                cues.push(String::new());
            }
            let mut rendered = cues.join("\n");
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            Ok(rendered)
        }
    }
}

/// Render one or more immutable session snapshots for clipboard transfer.
pub fn render_sessions_for_clipboard(
    sessions: &[ExportSession],
    format: ExportFormat,
) -> Result<String, ExportError> {
    let sessions = unique_sessions(sessions)?;
    if format == ExportFormat::Json {
        let mut rendered = serde_json::to_string_pretty(&sessions)?;
        rendered.push('\n');
        return Ok(rendered);
    }
    let separator = if format == ExportFormat::Markdown {
        "  \n"
    } else {
        "\n\n"
    };
    sessions
        .into_iter()
        .map(|session| render_session(session, format))
        .collect::<Result<Vec<_>, _>>()
        .map(|blocks| blocks.join(separator))
}

/// Render one session as pretty, deterministic JSON.
pub fn format_json(session: &ExportSession) -> Result<String, ExportError> {
    let mut rendered = serde_json::to_string_pretty(session)?;
    rendered.push('\n');
    Ok(rendered)
}

/// Render and atomically publish a single session or a multi-session ZIP.
#[cfg(test)]
pub fn export_sessions(
    sessions: &[ExportSession],
    format: ExportFormat,
    destination: impl AsRef<Path>,
    cancellation: Option<&CancellationToken>,
) -> Result<ExportResult, ExportError> {
    export_sessions_with_progress(sessions, format, destination, cancellation, |_| {})
}

/// Render and atomically publish sessions while reporting durable output stages.
pub fn export_sessions_with_progress(
    sessions: &[ExportSession],
    format: ExportFormat,
    destination: impl AsRef<Path>,
    cancellation: Option<&CancellationToken>,
    mut report: impl FnMut(ExportStage),
) -> Result<ExportResult, ExportError> {
    let sessions = unique_sessions(sessions)?;
    check_cancelled(cancellation)?;
    let destination = destination.as_ref().to_path_buf();
    let mut staging = StagingFile::create(&destination)?;
    report(ExportStage::Writing);

    {
        let file = staging.file.as_mut().expect("staging file is open");
        if sessions.len() == 1 {
            let content = render_session(sessions[0], format)?;
            check_cancelled(cancellation)?;
            file.write_all(content.as_bytes())?;
        } else {
            write_zip(file, &sessions, format, cancellation)?;
        }
    }

    check_cancelled(cancellation)?;
    if let Some(file) = staging.file.as_mut() {
        file.flush()?;
        file.sync_all()?;
    }
    check_cancelled(cancellation)?;
    report(ExportStage::Publishing);
    staging.close();
    staging.publish(&destination)?;
    sync_parent_directory(&destination)?;

    Ok(ExportResult {
        destination,
        exported_session_ids: sessions
            .iter()
            .map(|session| session.session_id.clone())
            .collect(),
    })
}

fn write_zip(
    file: &mut File,
    sessions: &[&ExportSession],
    format: ExportFormat,
    cancellation: Option<&CancellationToken>,
) -> Result<(), ExportError> {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut archive = ZipWriter::new(file);
    let mut manifest = Vec::with_capacity(sessions.len());
    for session in sessions {
        check_cancelled(cancellation)?;
        let filename = format!(
            "{}-{}.{}",
            safe_name(&session.title),
            safe_name(&session.session_id),
            format.extension()
        );
        let rendered = render_session(session, format)?;
        archive.start_file(&filename, options)?;
        archive.write_all(rendered.as_bytes())?;
        manifest.push(ManifestEntry {
            session_id: session.session_id.clone(),
            file: filename,
        });
    }
    check_cancelled(cancellation)?;
    archive.start_file("manifest.json", options)?;
    archive.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    archive.finish()?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestEntry {
    session_id: String,
    file: String,
}

fn unique_sessions(sessions: &[ExportSession]) -> Result<Vec<&ExportSession>, ExportError> {
    if sessions.is_empty() {
        return Err(ExportError::EmptySelection);
    }
    let mut seen = HashSet::with_capacity(sessions.len());
    let mut unique = Vec::with_capacity(sessions.len());
    for session in sessions {
        validate_session(session)?;
        if seen.insert(session.session_id.clone()) {
            unique.push(session);
        }
    }
    Ok(unique)
}

fn validate_session(session: &ExportSession) -> Result<(), ExportError> {
    if session.sample_rate == 0 {
        return Err(ExportError::InvalidSampleRate {
            session_id: session.session_id.clone(),
        });
    }
    let mut previous_index = None;
    let mut previous_end = 0;
    for segment in &session.segments {
        if segment.end_sample <= segment.start_sample || segment.start_sample < previous_end {
            return Err(ExportError::InvalidSegmentRange {
                session_id: session.session_id.clone(),
            });
        }
        if previous_index.is_some_and(|index| segment.segment_index <= index) {
            return Err(ExportError::UnorderedSegments {
                session_id: session.session_id.clone(),
            });
        }
        previous_index = Some(segment.segment_index);
        previous_end = segment.end_sample;
    }
    Ok(())
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

#[derive(Clone, Copy)]
enum TimestampStyle {
    Srt,
    Vtt,
}

fn format_timestamp(
    samples: u64,
    sample_rate: u32,
    style: TimestampStyle,
) -> Result<String, ExportError> {
    if sample_rate == 0 {
        return Err(ExportError::InvalidSampleRate {
            session_id: String::new(),
        });
    }
    let milliseconds =
        (u128::from(samples) * 1_000 + u128::from(sample_rate) / 2) / u128::from(sample_rate);
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    let separator = match style {
        TimestampStyle::Srt => ',',
        TimestampStyle::Vtt => '.',
    };
    Ok(format!(
        "{hours:02}:{minutes:02}:{seconds:02}{separator}{millis:03}"
    ))
}

fn safe_name(value: &str) -> String {
    let mut result = String::with_capacity(value.len().min(80));
    for character in value.chars() {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            result.push(character);
        } else {
            result.push('-');
        }
        if result.len() >= 80 {
            break;
        }
    }
    let result = result.trim_matches('-').to_owned();
    if result.is_empty() {
        "transcript".to_owned()
    } else {
        result
    }
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), ExportError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

struct StagingFile {
    path: PathBuf,
    file: Option<File>,
}

impl StagingFile {
    fn create(destination: &Path) -> Result<Self, ExportError> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let filename = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
            })?;
        for attempt in 0..100u32 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = parent.join(format!(".{filename}.{nonce}-{attempt}.tmp"));
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an export staging file",
        )
        .into())
    }

    fn close(&mut self) {
        self.file.take();
    }

    fn publish(&mut self, destination: &Path) -> Result<(), ExportError> {
        self.close();
        fs::rename(&self.path, destination)?;
        Ok(())
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn sync_parent_directory(destination: &Path) -> Result<(), ExportError> {
    #[cfg(unix)]
    {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[derive(Deserialize)]
    struct Fixture {
        session: ExportSession,
        expected: Expected,
        #[serde(rename = "multiSessionZip")]
        multi_session_zip: ZipExpectation,
    }

    #[derive(Deserialize)]
    struct Expected {
        txt: String,
        #[serde(rename = "timestampedTxt")]
        timestamped_txt: String,
        markdown: String,
        srt: String,
        vtt: String,
    }

    #[derive(Deserialize)]
    struct ZipExpectation {
        format: String,
        entries: Vec<String>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../../fixtures/native/export-cases.json"))
            .expect("native export fixture must be valid")
    }

    #[test]
    fn render_fixture_formats() {
        let fixture = fixture();
        assert_eq!(
            render_session(&fixture.session, ExportFormat::Txt).unwrap(),
            fixture.expected.txt
        );
        assert_eq!(
            render_session(&fixture.session, ExportFormat::TimestampedTxt).unwrap(),
            fixture.expected.timestamped_txt
        );
        assert_eq!(
            render_session(&fixture.session, ExportFormat::Markdown).unwrap(),
            fixture.expected.markdown
        );
        assert_eq!(
            render_session(&fixture.session, ExportFormat::Srt).unwrap(),
            fixture.expected.srt
        );
        assert_eq!(
            render_session(&fixture.session, ExportFormat::Vtt).unwrap(),
            fixture.expected.vtt
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_session(&fixture.session, ExportFormat::Json).unwrap())
                .unwrap();
        assert_eq!(json["sessionId"], "session-file");
        assert_eq!(json["segments"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn clipboard_render_supports_multiple_sessions() {
        let fixture = fixture();
        let mut second = fixture.session.clone();
        second.session_id = "session-second".into();
        second.title = "Second".into();

        let text = render_sessions_for_clipboard(
            &[fixture.session.clone(), second.clone()],
            ExportFormat::Txt,
        )
        .unwrap();
        assert_eq!(text, "hello\nworld\n\nhello\nworld");

        let json =
            render_sessions_for_clipboard(&[fixture.session, second], ExportFormat::Json).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn multi_session_zip_has_selected_format_and_manifest() {
        let fixture = fixture();
        let second = ExportSession {
            session_id: "session-second".into(),
            title: "Second".into(),
            sample_rate: 16_000,
            segments: vec![ExportSegment {
                segment_index: 0,
                start_sample: 0,
                end_sample: 16_000,
                text: "second".into(),
                language: None,
                split_reason: None,
                raw_text: None,
                diagnostics: None,
            }],
        };
        let format = match fixture.multi_session_zip.format.as_str() {
            "markdown" => ExportFormat::Markdown,
            value => panic!("unsupported fixture format: {value}"),
        };
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("transcripts.zip");
        export_sessions(&[fixture.session, second], format, &destination, None).unwrap();

        let file = File::open(destination).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        let mut expected = fixture.multi_session_zip.entries;
        expected.sort();
        assert_eq!(names, expected);

        let mut manifest = String::new();
        archive
            .by_name("manifest.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(manifest.as_array().unwrap().len(), 2);
        assert_eq!(manifest[0]["sessionId"], "session-file");
    }

    #[test]
    fn cancellation_preserves_destination_and_removes_staging() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("transcript.txt");
        fs::write(&destination, b"original").expect("fixture destination should be writable");
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            export_sessions(
                &[fixture.session],
                ExportFormat::Txt,
                &destination,
                Some(&cancellation)
            ),
            Err(ExportError::Cancelled)
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_snapshot_never_creates_or_replaces_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("transcript.txt");
        fs::write(&destination, b"original").unwrap();
        let invalid = ExportSession {
            session_id: "invalid".into(),
            title: "Invalid".into(),
            sample_rate: 0,
            segments: Vec::new(),
        };

        assert!(matches!(
            export_sessions(&[invalid], ExportFormat::Txt, &destination, None),
            Err(ExportError::InvalidSampleRate { .. })
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn successful_publish_replaces_destination_atomically() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("transcript.txt");
        fs::write(&destination, b"old").unwrap();

        export_sessions(&[fixture.session], ExportFormat::Txt, &destination, None).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"hello\nworld");
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn progress_reports_write_before_atomic_publish() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("transcript.txt");
        let mut stages = Vec::new();

        export_sessions_with_progress(
            &[fixture.session],
            ExportFormat::Txt,
            &destination,
            None,
            |stage| stages.push(stage),
        )
        .unwrap();

        assert_eq!(stages, vec![ExportStage::Writing, ExportStage::Publishing]);
        assert_eq!(fs::read_to_string(destination).unwrap(), "hello\nworld");
    }
}
