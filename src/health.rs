//! What the Health page shows: yt-dlp and tool versions, a playback check, and the
//! recent problems.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::errlog::{ErrorLog, LogEntry};
use crate::history::History;
use crate::source::YtDlpFfmpegSource;
use crate::ytdlp::{UpdateState, YtDlp, tool_version};

/// The song checked when nothing has been played yet.
const FALLBACK_SONG: &str = "dQw4w9WgXcQ";
/// How long a tool version (ffmpeg, deno) is trusted.
const TOOL_TTL: Duration = Duration::from_secs(10 * 60);
/// Longest tool version line kept.
const TOOL_LINE_MAX: usize = 80;

/// The result of one playback check.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub ok: bool,
    pub video_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How long it took.
    pub ms: u64,
    pub at: String,
}

/// The current output, as the page shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputSummary {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PinsonInfo {
    version: &'static str,
    started_at: String,
    uptime_secs: u64,
}

#[derive(Debug, Serialize)]
struct YtDlpView {
    path: String,
    version: Option<String>,
    latest: Option<String>,
    managed: bool,
    update: UpdateState,
}

#[derive(Debug, Serialize)]
struct ToolView {
    name: &'static str,
    version: Option<String>,
}

/// The body of `GET /api/health`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pinson: PinsonInfo,
    yt_dlp: YtDlpView,
    tools: Vec<ToolView>,
    output: Option<OutputSummary>,
    check: Option<CheckResult>,
    problems: Vec<LogEntry>,
}

struct Inner {
    errors: ErrorLog,
    ytdlp: YtDlp,
    source: Option<Arc<YtDlpFfmpegSource>>,
    ffmpeg: PathBuf,
    deno: PathBuf,
    started_at: DateTime<Utc>,
    started: Instant,
    last_check: Mutex<Option<CheckResult>>,
    tools: Mutex<HashMap<&'static str, (Instant, Option<String>)>>,
}

/// Shared state behind the Health page. Cheap to clone.
#[derive(Clone)]
pub struct Health {
    inner: Arc<Inner>,
}

impl Health {
    /// `source` runs the playback check; `ffmpeg` and `deno` are the tools whose versions show.
    pub fn new(
        errors: ErrorLog,
        ytdlp: YtDlp,
        source: Option<Arc<YtDlpFfmpegSource>>,
        ffmpeg: PathBuf,
        deno: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                errors,
                ytdlp,
                source,
                ffmpeg,
                deno,
                started_at: Utc::now(),
                started: Instant::now(),
                last_check: Mutex::new(None),
                tools: Mutex::default(),
            }),
        }
    }

    /// A Health that runs no real tool and makes no network call: every version is
    /// unknown and the playback check fails.
    pub fn for_tests() -> Self {
        Self::for_tests_with(None)
    }

    /// Like `for_tests`, with a source that runs the playback check.
    pub fn for_tests_with(source: Option<Arc<YtDlpFfmpegSource>>) -> Self {
        let missing = PathBuf::from("pinson-test-missing-tool");
        let ytdlp = YtDlp::new(
            missing.clone().into(),
            missing.clone(),
            std::env::temp_dir().join("pinson-test-missing-yt-dlp"),
            missing.clone(),
            None,
        );
        Self::new(ErrorLog::default(), ytdlp, source, missing.clone(), missing)
    }

    /// The yt-dlp manager (startup choice and updates).
    pub fn ytdlp(&self) -> &YtDlp {
        &self.inner.ytdlp
    }

    /// The recent-problems log.
    pub fn errors(&self) -> &ErrorLog {
        &self.inner.errors
    }

    /// Resolves one song (the newest play, else a fallback) without downloading it, and
    /// keeps the result.
    pub async fn check(&self, history: &History) -> CheckResult {
        let video_id = history
            .recent(1)
            .ok()
            .and_then(|songs| songs.into_iter().next())
            .map_or_else(|| FALLBACK_SONG.to_owned(), |s| s.video_id);
        let started = Instant::now();
        let outcome = match &self.inner.source {
            Some(source) => source.check(&video_id).await,
            None => Err(anyhow::anyhow!("no yt-dlp source")),
        };
        let (title, error) = match outcome {
            Ok(title) => (Some(title), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let result = CheckResult {
            ok: title.is_some(),
            video_id,
            title,
            error,
            ms: started.elapsed().as_millis() as u64,
            at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        *self.inner.last_check.lock().unwrap() = Some(result.clone());
        result
    }

    async fn tool(&self, name: &'static str, bin: &Path, arg: &str) -> ToolView {
        if let Some((at, v)) = self.inner.tools.lock().unwrap().get(name)
            && at.elapsed() < TOOL_TTL
        {
            return ToolView {
                name,
                version: v.clone(),
            };
        }
        let version = tool_version(bin, arg)
            .await
            .map(|line| line.chars().take(TOOL_LINE_MAX).collect::<String>());
        self.inner
            .tools
            .lock()
            .unwrap()
            .insert(name, (Instant::now(), version.clone()));
        ToolView { name, version }
    }

    /// Everything the page shows; `output` is the current output, if any.
    pub async fn report(&self, output: Option<OutputSummary>) -> Report {
        let (info, ffmpeg, deno) = tokio::join!(
            self.inner.ytdlp.info(),
            self.tool("ffmpeg", &self.inner.ffmpeg, "-version"),
            self.tool("deno", &self.inner.deno, "--version"),
        );
        Report {
            pinson: PinsonInfo {
                version: env!("CARGO_PKG_VERSION"),
                started_at: self
                    .inner
                    .started_at
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                uptime_secs: self.inner.started.elapsed().as_secs(),
            },
            yt_dlp: YtDlpView {
                path: info.path.display().to_string(),
                version: info.version,
                latest: info.latest,
                managed: info.managed,
                update: self.inner.ytdlp.state(),
            },
            tools: vec![ffmpeg, deno],
            output,
            check: self.inner.last_check.lock().unwrap().clone(),
            problems: self.inner.errors.entries(),
        }
    }
}
