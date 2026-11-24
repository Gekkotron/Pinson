//! PCM source: `yt-dlp -o - <url> | ffmpeg … -f s16le -ar 48000 -ac 2 -`.

use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::loudness::{Leveled, Loudness};
use crate::offline::{OfflineIndex, valid_video_id};
use crate::ytdlp::YtDlpBin;

/// Reads raw PCM: s16le, 48 kHz, 2 channels.
///
/// `Any` lets test engines recognise their own fake readers.
#[async_trait]
pub trait PcmReader: Any + Send + 'static {
    /// Returns the number of bytes read; 0 means the song has ended.
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;
}

#[async_trait]
pub trait PcmSource: Send + Sync + 'static {
    /// Starts the song `start` into it and returns once its first PCM bytes
    /// have arrived.
    async fn open(&self, video_id: &str, start: Duration) -> anyhow::Result<Box<dyn PcmReader>>;

    /// Gets `video_id` ready in the background so a later `open` starts
    /// sooner. Never fails; the default does nothing.
    fn prepare(&self, _video_id: &str) {}
}

pub struct YtDlpFfmpegSource {
    /// The yt-dlp to run, read at every spawn so an update applies to the next song.
    pub yt_dlp: YtDlpBin,
    pub ffmpeg: PathBuf,
    pub cookies_from_browser: Option<String>,
    pub cookies_file: Option<PathBuf>,
    /// YouTube clients yt-dlp asks for the stream (its `player_client`), e.g.
    /// `visionos`; `None` leaves yt-dlp's default. If a song fails with them,
    /// it is opened again with yt-dlp's default.
    pub player_clients: Option<String>,
    /// Loudness normalisation; `None` plays the song as decoded.
    pub loudness: Option<Loudness>,
    /// Where each song's resolved YouTube info is kept (`<id>.info.json`), so
    /// a seek, Previous or a prepared song skips yt-dlp's extraction; `None`
    /// keeps no info.
    pub info_dir: Option<PathBuf>,
    /// Favorites saved on disk, played without yt-dlp; `None` keeps none.
    offline: Option<OfflineIndex>,
    /// Songs being prepared, each with a flag that turns true when done.
    in_flight: Arc<Mutex<HashMap<String, watch::Receiver<bool>>>>,
}

/// Longest the playback check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a saved info file is used. YouTube stream URLs last about 6 h.
pub const INFO_TTL: Duration = Duration::from_secs(5 * 3600);
/// Most info files kept in `info_dir`.
const INFO_MAX_FILES: usize = 64;
/// How long a background prepare may run.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);

/// Removes stale info files from `dir`, then the oldest ones beyond `max`.
fn sweep_info_dir(dir: &Path, ttl: Duration, max: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".info.json") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if is_fresh(modified, ttl) {
            files.push((modified, path));
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    if files.len() > max {
        files.sort();
        for (_, path) in &files[..files.len() - max] {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn is_fresh(modified: SystemTime, ttl: Duration) -> bool {
    modified.elapsed().map_or(true, |age| age < ttl)
}

/// How long `open` waits for the first PCM bytes.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for the processes to exit once their output has ended.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// `ffmpeg` arguments: decode stdin to PCM, dropping the first `start`.
pub fn ffmpeg_args(start: Duration) -> Vec<String> {
    let mut args: Vec<String> = ["-nostdin", "-loglevel", "error", "-i", "pipe:0"]
        .map(String::from)
        .into();
    if !start.is_zero() {
        // Output seeking: the input is a pipe, so ffmpeg decodes and drops
        // everything before `start` (sample-accurate).
        args.extend(["-ss".into(), format!("{:.3}", start.as_secs_f64())]);
    }
    args.extend(["-f", "s16le", "-ar", "48000", "-ac", "2", "-"].map(String::from));
    args
}

impl YtDlpFfmpegSource {
    pub fn new(
        yt_dlp: YtDlpBin,
        ffmpeg: PathBuf,
        cookies_from_browser: Option<String>,
        cookies_file: Option<PathBuf>,
        player_clients: Option<String>,
        loudness: Option<Loudness>,
        info_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            yt_dlp,
            ffmpeg,
            cookies_from_browser,
            cookies_file,
            player_clients,
            loudness,
            info_dir,
            offline: None,
            in_flight: Arc::default(),
        }
    }

    /// Plays songs found in `index` from their saved file.
    pub fn with_offline(mut self, index: OfflineIndex) -> Self {
        self.offline = Some(index);
        self
    }

    fn cookie_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(browser) = &self.cookies_from_browser {
            args.extend(["--cookies-from-browser".into(), browser.clone()]);
        }
        if let Some(file) = &self.cookies_file {
            args.extend(["--cookies".into(), file.display().to_string()]);
        }
        args
    }

    /// `--write-info-json -o infojson:<dir>/%(id)s`, when info is kept.
    fn write_info_args(&self) -> Vec<String> {
        let Some(dir) = &self.info_dir else {
            return Vec::new();
        };
        vec![
            "--write-info-json".into(),
            "-o".into(),
            format!("infojson:{}/%(id)s", dir.display()),
        ]
    }

    pub fn yt_dlp_args(&self, video_id: &str, player_clients: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = ["-q", "-f", "bestaudio", "--no-playlist"]
            .map(String::from)
            .into();
        if let Some(clients) = player_clients {
            args.extend([
                "--extractor-args".into(),
                format!("youtube:player_client={clients}"),
            ]);
        }
        args.extend(self.cookie_args());
        args.extend(self.write_info_args());
        args.extend([
            "-o".into(),
            "-".into(),
            format!("https://music.youtube.com/watch?v={video_id}"),
        ]);
        args
    }

    /// Streams a song from its saved info file: no extraction, no URL.
    pub fn load_info_args(&self, info_file: &Path) -> Vec<String> {
        let mut args: Vec<String> = ["-q", "-f", "bestaudio", "--load-info-json"]
            .map(String::from)
            .into();
        args.push(info_file.display().to_string());
        args.extend(self.cookie_args());
        args.extend(["-o".into(), "-".into()]);
        args
    }

    /// Resolves a song and saves its info without downloading it.
    pub fn prepare_args(&self, video_id: &str) -> Vec<String> {
        let mut args: Vec<String> = ["-q", "-f", "bestaudio", "--no-playlist"]
            .map(String::from)
            .into();
        if let Some(clients) = self.player_clients.as_deref() {
            args.extend([
                "--extractor-args".into(),
                format!("youtube:player_client={clients}"),
            ]);
        }
        args.extend(self.cookie_args());
        args.push("--skip-download".into());
        args.extend(self.write_info_args());
        args.push(format!("https://music.youtube.com/watch?v={video_id}"));
        args
    }

    /// Downloads a song's best audio as it is (no re-encode) to `out_template`,
    /// a yt-dlp output template such as `<dir>/<id>.%(ext)s`.
    pub fn download_args(
        &self,
        video_id: &str,
        out_template: &str,
        player_clients: Option<&str>,
    ) -> Vec<String> {
        let mut args: Vec<String> = ["-q", "-f", "bestaudio", "--no-playlist"]
            .map(String::from)
            .into();
        if let Some(clients) = player_clients {
            args.extend([
                "--extractor-args".into(),
                format!("youtube:player_client={clients}"),
            ]);
        }
        args.extend(self.cookie_args());
        args.extend([
            "-o".into(),
            out_template.into(),
            format!("https://music.youtube.com/watch?v={video_id}"),
        ]);
        args
    }

    /// Downloads a song's audio into `tmp_dir` and returns the file written
    /// (`<id>.<ext>`). Like playback, a failure with the player clients is
    /// retried once with yt-dlp's default. Dropping the future kills yt-dlp.
    pub async fn download(&self, video_id: &str, tmp_dir: &Path) -> anyhow::Result<PathBuf> {
        if !valid_video_id(video_id) {
            bail!("{video_id:?} is not a video id");
        }
        tokio::fs::create_dir_all(tmp_dir)
            .await
            .with_context(|| format!("creating {}", tmp_dir.display()))?;
        remove_partial_downloads(tmp_dir, video_id);
        // `%` is special in yt-dlp output templates.
        let template = format!(
            "{}/{video_id}.%(ext)s",
            tmp_dir.display().to_string().replace('%', "%%")
        );
        let clients = self.player_clients.as_deref();
        match self
            .run_download(self.download_args(video_id, &template, clients))
            .await
        {
            Ok(()) => {}
            Err(e) if clients.is_some() => {
                tracing::warn!(
                    "{video_id}: download failed with player_client={}: {e:#}; retrying with yt-dlp's default",
                    clients.unwrap_or_default()
                );
                self.run_download(self.download_args(video_id, &template, None))
                    .await?;
            }
            Err(e) => return Err(e),
        }
        finished_download(tmp_dir, video_id)
            .ok_or_else(|| anyhow!("yt-dlp wrote no file for {video_id}"))
    }

    /// Runs one `yt-dlp` to the end.
    async fn run_download(&self, args: Vec<String>) -> anyhow::Result<()> {
        let yt_dlp_bin = self.yt_dlp.get();
        let mut child = Command::new(&yt_dlp_bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", yt_dlp_bin.display()))?;
        let mut err = LastLine::collect(child.stderr.take());
        let status = child.wait().await.context("waiting for yt-dlp")?;
        if !status.success() {
            bail!("yt-dlp failed: {}", err.get().await);
        }
        Ok(())
    }

    /// Where the song's info file lives; `None` when info isn't kept or the
    /// id could not be a YouTube video id (it becomes part of a file name).
    pub fn info_path(&self, video_id: &str) -> Option<PathBuf> {
        let dir = self.info_dir.as_ref()?;
        valid_video_id(video_id).then(|| dir.join(format!("{video_id}.info.json")))
    }

    fn fresh_info(&self, video_id: &str) -> Option<PathBuf> {
        let path = self.info_path(video_id)?;
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        is_fresh(modified, INFO_TTL).then_some(path)
    }

    fn sweep(&self) {
        if let Some(dir) = &self.info_dir {
            sweep_info_dir(dir, INFO_TTL, INFO_MAX_FILES);
        }
    }

    /// Arguments that resolve a song and print its title, without downloading it.
    pub fn check_args(&self, video_id: &str, player_clients: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = [
            "-f",
            "bestaudio",
            "--no-playlist",
            "--simulate",
            "--no-warnings",
            "--print",
            "title",
        ]
        .map(String::from)
        .into();
        if let Some(clients) = player_clients {
            args.extend([
                "--extractor-args".into(),
                format!("youtube:player_client={clients}"),
            ]);
        }
        args.extend(self.cookie_args());
        args.push(format!("https://music.youtube.com/watch?v={video_id}"));
        args
    }

    /// Resolves a song like playback does, without downloading it, and returns its title.
    /// If the configured player clients fail, tries yt-dlp's default like `open`.
    pub async fn check(&self, video_id: &str) -> anyhow::Result<String> {
        let clients = self.player_clients.as_deref();
        match self.run_check(self.check_args(video_id, clients)).await {
            Err(_) if clients.is_some() => self.run_check(self.check_args(video_id, None)).await,
            result => result,
        }
    }

    async fn run_check(&self, args: Vec<String>) -> anyhow::Result<String> {
        let bin = self.yt_dlp.get();
        let output = tokio::time::timeout(
            CHECK_TIMEOUT,
            Command::new(&bin)
                .args(args)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("no answer after {} s", CHECK_TIMEOUT.as_secs()))?
        .with_context(|| format!("starting {}", bin.display()))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            let last = err.lines().map(str::trim).rfind(|l| !l.is_empty());
            bail!("{}", tidy_error(last.unwrap_or("yt-dlp failed")));
        }
        let out = String::from_utf8_lossy(&output.stdout);
        match out.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(title) => Ok(title.to_owned()),
            None => bail!("yt-dlp printed no title"),
        }
    }

    /// Waits (up to `OPEN_TIMEOUT`) for a running prepare of `video_id`.
    async fn wait_for_prepare(&self, video_id: &str) {
        let rx = self.in_flight.lock().unwrap().get(video_id).cloned();
        if let Some(mut rx) = rx {
            let _ = tokio::time::timeout(OPEN_TIMEOUT, rx.wait_for(|done| *done)).await;
        }
    }

    /// Starts `yt-dlp | ffmpeg` and waits for the first PCM bytes.
    pub async fn open_pipeline(
        &self,
        video_id: &str,
        start: Duration,
        player_clients: Option<&str>,
    ) -> anyhow::Result<PipelineReader> {
        self.spawn_pipeline(self.yt_dlp_args(video_id, player_clients), start)
            .await
    }

    /// Starts `yt-dlp <yt_dlp_args> | ffmpeg` and waits for the first PCM bytes.
    async fn spawn_pipeline(
        &self,
        yt_dlp_args: Vec<String>,
        start: Duration,
    ) -> anyhow::Result<PipelineReader> {
        let yt_dlp_bin = self.yt_dlp.get();
        let mut yt_dlp = Command::new(&yt_dlp_bin)
            .args(yt_dlp_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", yt_dlp_bin.display()))?;
        let yt_dlp_out: Stdio = yt_dlp.stdout.take().expect("piped stdout").try_into()?;
        let yt_dlp_err = LastLine::collect(yt_dlp.stderr.take());
        let mut ffmpeg = Command::new(&self.ffmpeg)
            .args(ffmpeg_args(start))
            .stdin(yt_dlp_out)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", self.ffmpeg.display()))?;
        let ffmpeg_err = LastLine::collect(ffmpeg.stderr.take());
        let stdout = ffmpeg.stdout.take().expect("piped stdout");
        Self::first_bytes(PipelineReader {
            yt_dlp: Some(yt_dlp),
            ffmpeg,
            stdout,
            yt_dlp_err,
            ffmpeg_err,
            head: Vec::new(),
            head_pos: 0,
            ended: false,
        })
        .await
    }

    /// Decodes a saved file with `ffmpeg` alone and waits for the first PCM bytes.
    async fn spawn_file_pipeline(
        &self,
        file: &Path,
        start: Duration,
    ) -> anyhow::Result<PipelineReader> {
        let mut args: Vec<String> = ["-nostdin", "-loglevel", "error"].map(String::from).into();
        if !start.is_zero() {
            // Input seeking: jumps straight to `start` in the file.
            args.extend(["-ss".into(), format!("{:.3}", start.as_secs_f64())]);
        }
        args.extend(["-i".into(), file.display().to_string()]);
        args.extend(["-f", "s16le", "-ar", "48000", "-ac", "2", "-"].map(String::from));
        let mut ffmpeg = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", self.ffmpeg.display()))?;
        let ffmpeg_err = LastLine::collect(ffmpeg.stderr.take());
        let stdout = ffmpeg.stdout.take().expect("piped stdout");
        Self::first_bytes(PipelineReader {
            yt_dlp: None,
            ffmpeg,
            stdout,
            yt_dlp_err: LastLine::collect(None),
            ffmpeg_err,
            head: Vec::new(),
            head_pos: 0,
            ended: false,
        })
        .await
    }

    /// Waits (up to `OPEN_TIMEOUT`) for `reader`'s first PCM bytes.
    async fn first_bytes(mut reader: PipelineReader) -> anyhow::Result<PipelineReader> {
        let mut first = vec![0u8; 16 * 1024];
        let n = match tokio::time::timeout(OPEN_TIMEOUT, reader.stdout.read(&mut first)).await {
            Ok(read) => read.context("reading ffmpeg output")?,
            Err(_) => bail!("no audio after {} s", OPEN_TIMEOUT.as_secs()),
        };
        if n == 0 {
            return Err(reader
                .exit_error()
                .await
                .unwrap_or_else(|| anyhow!("the song has no audio")));
        }
        first.truncate(n);
        reader.head = first;
        Ok(reader)
    }
}

#[async_trait]
impl PcmSource for YtDlpFfmpegSource {
    async fn open(&self, video_id: &str, start: Duration) -> anyhow::Result<Box<dyn PcmReader>> {
        let reader = self.open_reader(video_id, start).await?;
        let Some(loudness) = &self.loudness else {
            return Ok(Box::new(reader));
        };
        let leveled = Leveled::start(
            Box::new(reader),
            loudness.cache.get(video_id),
            start,
            loudness.target_lufs,
        )
        .await;
        leveled.log_start(video_id);
        Ok(Box::new(leveled))
    }

    fn prepare(&self, video_id: &str) {
        let (Some(info_dir), Some(path)) = (self.info_dir.clone(), self.info_path(video_id)) else {
            return;
        };
        if self.fresh_info(video_id).is_some() {
            return;
        }
        let (done_tx, done_rx) = watch::channel(false);
        {
            let mut in_flight = self.in_flight.lock().unwrap();
            if in_flight.contains_key(video_id) {
                return;
            }
            in_flight.insert(video_id.to_owned(), done_rx);
        }
        let yt_dlp = self.yt_dlp.get();
        let args = self.prepare_args(video_id);
        let in_flight = self.in_flight.clone();
        let video_id = video_id.to_owned();
        tokio::spawn(async move {
            let result = run_prepare(&yt_dlp, args).await;
            match result {
                Ok(()) if path.exists() => sweep_info_dir(&info_dir, INFO_TTL, INFO_MAX_FILES),
                Ok(()) => tracing::debug!("{video_id}: prepare saved no info"),
                Err(e) => tracing::debug!("{video_id}: prepare failed: {e:#}"),
            }
            in_flight.lock().unwrap().remove(&video_id);
            let _ = done_tx.send(true);
        });
    }
}

impl YtDlpFfmpegSource {
    /// The `yt-dlp | ffmpeg` pipeline for a song: from its saved info when
    /// there is a fresh file, else with a full extraction.
    async fn open_reader(&self, video_id: &str, start: Duration) -> anyhow::Result<PipelineReader> {
        if let Some(index) = &self.offline {
            while let Some(file) = index.path(video_id) {
                match self.spawn_file_pipeline(&file, start).await {
                    Ok(reader) => {
                        tracing::info!("{video_id}: opened from the offline copy");
                        return Ok(reader);
                    }
                    Err(e) => {
                        index.discard(video_id, &file);
                        tracing::warn!("{video_id}: offline copy failed: {e:#}");
                        if index.path(video_id).as_ref() == Some(&file) {
                            break;
                        }
                    }
                }
            }
        }
        if self.info_dir.is_some() {
            self.wait_for_prepare(video_id).await;
        }
        if let Some(info_file) = self.fresh_info(video_id) {
            match self
                .spawn_pipeline(self.load_info_args(&info_file), start)
                .await
            {
                Ok(reader) => {
                    tracing::info!("{video_id}: opened from saved info");
                    return Ok(reader);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&info_file);
                    tracing::warn!("{video_id}: saved info failed: {e:#}; extracting again");
                }
            }
        }
        let clients = self.player_clients.as_deref();
        let reader = match self.open_pipeline(video_id, start, clients).await {
            Ok(reader) => reader,
            // These clients skip the default ones' wait for a pre-roll ad, but
            // YouTube may refuse them: retry once with yt-dlp's default.
            Err(e) if clients.is_some() => {
                tracing::warn!(
                    "{video_id}: failed with player_client={}: {e:#}; retrying with yt-dlp's default",
                    clients.unwrap_or_default()
                );
                self.open_pipeline(video_id, start, None).await?
            }
            Err(e) => return Err(e),
        };
        self.sweep();
        Ok(reader)
    }
}

/// Deletes `<video_id>.*` files left in `dir` by an earlier download.
fn remove_partial_downloads(dir: &Path, video_id: &str) {
    let prefix = format!("{video_id}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The finished `<video_id>.<ext>` in `dir`, ignoring yt-dlp's partial files.
fn finished_download(dir: &Path, video_id: &str) -> Option<PathBuf> {
    let prefix = format!("{video_id}.");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            name.is_some_and(|n| {
                n.starts_with(&prefix)
                    && n[prefix.len()..].chars().all(|c| c.is_ascii_alphanumeric())
            })
        })
}

/// Runs one background `yt-dlp` that saves a song's info, within
/// `PREPARE_TIMEOUT`.
/// Drops yt-dlp's "please report this issue … yt-dlp -U" boilerplate from an error line.
pub fn tidy_error(line: &str) -> &str {
    line.split("; please report this issue")
        .next()
        .unwrap_or(line)
        .trim_end()
}

async fn run_prepare(yt_dlp: &Path, args: Vec<String>) -> anyhow::Result<()> {
    let mut child = Command::new(yt_dlp)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", yt_dlp.display()))?;
    let mut err = LastLine::collect(child.stderr.take());
    let status = match tokio::time::timeout(PREPARE_TIMEOUT, child.wait()).await {
        Ok(status) => status.context("waiting for yt-dlp")?,
        Err(_) => bail!("no answer after {} s", PREPARE_TIMEOUT.as_secs()),
    };
    if !status.success() {
        bail!("yt-dlp failed: {}", err.get().await);
    }
    Ok(())
}

/// A running `yt-dlp | ffmpeg` pipeline. Dropping it kills both processes.
pub struct PipelineReader {
    /// `None` when playing a saved file.
    yt_dlp: Option<Child>,
    ffmpeg: Child,
    stdout: ChildStdout,
    yt_dlp_err: LastLine,
    ffmpeg_err: LastLine,
    /// Bytes read by `open` that haven't been handed out yet.
    head: Vec<u8>,
    head_pos: usize,
    ended: bool,
}

impl PipelineReader {
    pub fn pids(&self) -> Vec<u32> {
        [self.yt_dlp.as_ref().and_then(Child::id), self.ffmpeg.id()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Waits for both processes and describes the first one that failed.
    async fn exit_error(&mut self) -> Option<anyhow::Error> {
        let yt_dlp = match &mut self.yt_dlp {
            Some(child) => Some(tokio::time::timeout(EXIT_TIMEOUT, child.wait()).await),
            None => None,
        };
        let ffmpeg = tokio::time::timeout(EXIT_TIMEOUT, self.ffmpeg.wait()).await;
        let failed = |status: &Result<std::io::Result<std::process::ExitStatus>, _>| !matches!(status, Ok(Ok(s)) if s.success());
        if yt_dlp.as_ref().is_some_and(failed) {
            Some(anyhow!("yt-dlp failed: {}", self.yt_dlp_err.get().await))
        } else if failed(&ffmpeg) {
            Some(anyhow!("ffmpeg failed: {}", self.ffmpeg_err.get().await))
        } else {
            None
        }
    }
}

#[async_trait]
impl PcmReader for PipelineReader {
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        if self.head_pos < self.head.len() {
            let n = buf.len().min(self.head.len() - self.head_pos);
            buf[..n].copy_from_slice(&self.head[self.head_pos..self.head_pos + n]);
            self.head_pos += n;
            return Ok(n);
        }
        if self.ended {
            return Ok(0);
        }
        let n = self
            .stdout
            .read(buf)
            .await
            .context("reading ffmpeg output")?;
        if n == 0 {
            self.ended = true;
            if let Some(err) = self.exit_error().await {
                return Err(err);
            }
        }
        Ok(n)
    }
}

/// Keeps the last non-empty stderr line of a process.
struct LastLine {
    line: Arc<Mutex<Option<String>>>,
    task: Option<JoinHandle<()>>,
}

impl LastLine {
    fn collect(stderr: Option<ChildStderr>) -> Self {
        let line = Arc::new(Mutex::new(None));
        let task = stderr.map(|stderr| {
            let line = line.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let l = l.trim();
                    if !l.is_empty() {
                        *line.lock().unwrap() = Some(l.to_owned());
                    }
                }
            })
        });
        Self { line, task }
    }

    /// The last line, once stderr has closed (or after a short wait).
    async fn get(&mut self) -> String {
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
        }
        self.line
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "no error output".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn source(yt_dlp: &str) -> YtDlpFfmpegSource {
        YtDlpFfmpegSource::new(yt_dlp.into(), "ffmpeg".into(), None, None, None, None, None)
    }

    /// A temp dir for one test, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pinson-source-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn info(&self, id: &str) -> PathBuf {
            self.0.join(format!("{id}.info.json"))
        }

        fn runs(&self, id: &str) -> usize {
            std::fs::read_to_string(self.0.join(format!("{id}.runs")))
                .map_or(0, |s| s.lines().count())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_with_info(dir: &TempDir) -> YtDlpFfmpegSource {
        let mut s = fake();
        s.info_dir = Some(dir.0.clone());
        s
    }

    fn age(path: &Path, by: Duration) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - by).unwrap();
    }

    async fn plays(source: &YtDlpFfmpegSource, id: &str, start: Duration) {
        let mut reader = source.open(id, start).await.expect("open");
        let mut buf = vec![0u8; 8192];
        assert!(reader.read(&mut buf).await.unwrap() > 0);
    }

    fn fake() -> YtDlpFfmpegSource {
        source(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-yt-dlp.sh"
        ))
    }

    fn fake_downloader() -> YtDlpFfmpegSource {
        source(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-yt-dlp-download.sh"
        ))
    }

    fn sine_file() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sine2s.wav"
        ))
    }

    async fn pcm_len(source: &YtDlpFfmpegSource, id: &str, start: Duration) -> usize {
        let mut reader = source.open(id, start).await.expect("open");
        let mut buf = vec![0u8; 8192];
        let mut total = 0;
        loop {
            let n = reader.read(&mut buf).await.expect("read");
            if n == 0 {
                return total;
            }
            total += n;
        }
    }

    #[tokio::test]
    async fn saved_file_plays_without_yt_dlp() {
        let index = OfflineIndex::default();
        index.insert("saved", sine_file(), 1);
        let s = source("/nonexistent/yt-dlp").with_offline(index);
        let total = pcm_len(&s, "saved", Duration::ZERO).await;
        let expected = 2 * 192_000;
        assert!(
            total.abs_diff(expected) <= expected / 100,
            "got {total} bytes"
        );
    }

    #[tokio::test]
    async fn saved_file_seeks_with_the_input() {
        let index = OfflineIndex::default();
        index.insert("saved", sine_file(), 1);
        let s = source("/nonexistent/yt-dlp").with_offline(index);
        let total = pcm_len(&s, "saved", Duration::from_secs(1)).await;
        let expected = 192_000;
        assert!(
            total.abs_diff(expected) <= expected / 20,
            "got {total} bytes"
        );
    }

    #[tokio::test]
    async fn broken_saved_file_is_deleted_and_youtube_is_used() {
        let dir = TempDir::new();
        let file = dir.0.join("ok.webm");
        std::fs::write(&file, b"this is not audio").unwrap();
        let index = OfflineIndex::default();
        index.insert("ok", file.clone(), 17);
        let s = fake().with_offline(index.clone());
        let total = pcm_len(&s, "ok", Duration::ZERO).await;
        assert!(total > 0, "played from the fake yt-dlp");
        assert!(!file.exists(), "the broken copy is deleted");
        assert!(index.path("ok").is_none());
    }

    #[tokio::test]
    async fn songs_without_a_copy_use_yt_dlp() {
        let s = fake().with_offline(OfflineIndex::default());
        assert!(pcm_len(&s, "other", Duration::ZERO).await > 0);
    }

    #[test]
    fn download_args_save_the_stream_untouched() {
        let mut s = source("yt-dlp");
        s.cookies_file = Some("/tmp/c.txt".into());
        assert_eq!(
            s.download_args("abc", "/d/abc.%(ext)s", Some("visionos")),
            [
                "-q",
                "-f",
                "bestaudio",
                "--no-playlist",
                "--extractor-args",
                "youtube:player_client=visionos",
                "--cookies",
                "/tmp/c.txt",
                "-o",
                "/d/abc.%(ext)s",
                "https://music.youtube.com/watch?v=abc"
            ]
        );
        assert!(
            !s.download_args("abc", "x", None)
                .contains(&"--extractor-args".to_owned())
        );
    }

    #[tokio::test]
    async fn download_returns_the_file_written() {
        let dir = TempDir::new();
        let file = fake_downloader()
            .download("ok", &dir.0)
            .await
            .expect("download");
        assert_eq!(file, dir.0.join("ok.webm"));
        assert_eq!(std::fs::read(&file).unwrap(), b"audio");
    }

    #[tokio::test]
    async fn download_retries_without_player_clients() {
        let dir = TempDir::new();
        let mut s = fake_downloader();
        s.player_clients = Some("visionos".into());
        assert!(s.download("noclient", &dir.0).await.is_ok());
    }

    #[tokio::test]
    async fn download_reports_errors_and_bad_ids() {
        let dir = TempDir::new();
        let s = fake_downloader();
        let err = s.download("bad", &dir.0).await.unwrap_err();
        assert!(err.to_string().contains("Video unavailable"), "{err:#}");
        assert!(s.download("../x", &dir.0).await.is_err());
    }

    #[test]
    fn ffmpeg_args_seek_only_when_asked() {
        let plain = ffmpeg_args(Duration::ZERO);
        assert!(!plain.contains(&"-ss".to_owned()));
        assert_eq!(plain.last().map(String::as_str), Some("-"));
        let seek = ffmpeg_args(Duration::from_millis(83_500));
        let ss = seek.iter().position(|a| a == "-ss").expect("-ss");
        assert_eq!(seek[ss + 1], "83.500");
        let input = seek.iter().position(|a| a == "pipe:0").unwrap();
        assert!(ss > input, "after -i: output seeking on the piped input");
    }

    #[test]
    fn tidy_error_drops_the_report_boilerplate() {
        assert_eq!(
            tidy_error(
                "ERROR: [youtube] abc: Sign in to confirm; please report this issue on  https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the appropriate issue template. Confirm you are on the latest version using  yt-dlp -U"
            ),
            "ERROR: [youtube] abc: Sign in to confirm"
        );
        assert_eq!(
            tidy_error("ERROR: Video unavailable"),
            "ERROR: Video unavailable"
        );
    }

    #[test]
    fn yt_dlp_args_basic() {
        assert_eq!(
            source("yt-dlp").yt_dlp_args("abc", None),
            [
                "-q",
                "-f",
                "bestaudio",
                "--no-playlist",
                "-o",
                "-",
                "https://music.youtube.com/watch?v=abc"
            ]
        );
    }

    #[test]
    fn yt_dlp_args_with_cookies_from_browser() {
        let mut s = source("yt-dlp");
        s.cookies_from_browser = Some("firefox".into());
        let args = s.yt_dlp_args("abc", None);
        assert_eq!(&args[4..6], ["--cookies-from-browser", "firefox"]);
        assert_eq!(
            args.last().unwrap(),
            "https://music.youtube.com/watch?v=abc"
        );
    }

    #[test]
    fn yt_dlp_args_with_cookies_file() {
        let mut s = source("yt-dlp");
        s.cookies_file = Some("/tmp/c.txt".into());
        let args = s.yt_dlp_args("abc", None);
        assert_eq!(&args[4..6], ["--cookies", "/tmp/c.txt"]);
    }

    #[test]
    fn yt_dlp_args_with_player_clients() {
        let mut s = source("yt-dlp");
        s.player_clients = Some("visionos".into());
        let args = s.yt_dlp_args("abc", s.player_clients.as_deref());
        assert_eq!(
            &args[4..6],
            ["--extractor-args", "youtube:player_client=visionos"]
        );
        assert_eq!(
            args.last().unwrap(),
            "https://music.youtube.com/watch?v=abc"
        );
        assert!(
            !s.yt_dlp_args("abc", None)
                .contains(&"--extractor-args".to_owned()),
            "no clients: yt-dlp's default"
        );
    }

    fn fake_with_clients() -> YtDlpFfmpegSource {
        let mut s = fake();
        s.player_clients = Some("visionos".into());
        s
    }

    #[tokio::test]
    async fn open_falls_back_to_default_clients() {
        let mut reader = fake_with_clients()
            .open("noclient", Duration::ZERO)
            .await
            .expect("plays with yt-dlp's default clients");
        let mut buf = vec![0u8; 8192];
        assert!(reader.read(&mut buf).await.unwrap() > 0);
    }

    #[tokio::test]
    async fn open_with_clients_still_reports_errors() {
        let err = fake_with_clients()
            .open("bad", Duration::ZERO)
            .await
            .err()
            .expect("open must fail");
        assert!(err.to_string().contains("Video unavailable"), "{err:#}");
    }

    #[tokio::test]
    async fn open_reads_whole_song() {
        let mut reader = fake().open("ok", Duration::ZERO).await.expect("open");
        let mut buf = vec![0u8; 8192];
        let mut total = 0usize;
        loop {
            let n = reader.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            total += n;
        }
        let expected = 2 * 192_000;
        assert!(
            total.abs_diff(expected) <= expected / 100,
            "got {total} bytes"
        );
        assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn open_reports_yt_dlp_error() {
        let err = fake()
            .open("bad", Duration::ZERO)
            .await
            .err()
            .expect("open must fail");
        assert!(err.to_string().contains("Video unavailable"), "{err:#}");
    }

    fn alive(pid: u32) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    #[tokio::test]
    async fn drop_kills_processes() {
        let reader = fake()
            .open_pipeline("endless", Duration::ZERO, None)
            .await
            .expect("open");
        let pids = reader.pids();
        assert_eq!(pids.len(), 2);
        assert!(pids.iter().all(|&p| alive(p)));
        drop(reader);
        for _ in 0..50 {
            if pids.iter().all(|&p| !alive(p)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!("children still alive: {pids:?}");
    }

    #[test]
    fn info_args_only_when_info_dir_is_set() {
        let dir = TempDir::new();
        let with = fake_with_info(&dir);
        let args = with.yt_dlp_args("abc", None);
        let n = args.len();
        assert_eq!(
            args[n - 6..n - 1],
            [
                "--write-info-json".to_owned(),
                "-o".into(),
                format!("infojson:{}/%(id)s", dir.0.display()),
                "-o".into(),
                "-".into(),
            ]
        );
        assert_eq!(
            args.last().unwrap(),
            "https://music.youtube.com/watch?v=abc"
        );
        let plain = source("yt-dlp").yt_dlp_args("abc", None);
        assert!(!plain.contains(&"--write-info-json".to_owned()));
    }

    #[test]
    fn load_info_args_have_no_url_or_extractor_args() {
        let mut s = source("yt-dlp");
        s.player_clients = Some("visionos".into());
        s.cookies_file = Some("/tmp/c.txt".into());
        let args = s.load_info_args(Path::new("/x/abc.info.json"));
        assert_eq!(
            args,
            [
                "-q",
                "-f",
                "bestaudio",
                "--load-info-json",
                "/x/abc.info.json",
                "--cookies",
                "/tmp/c.txt",
                "-o",
                "-"
            ]
        );
    }

    #[test]
    fn prepare_args_skip_the_download() {
        let dir = TempDir::new();
        let mut s = fake_with_info(&dir);
        s.player_clients = Some("visionos".into());
        let args = s.prepare_args("abc");
        assert!(args.contains(&"--skip-download".to_owned()));
        assert!(args.contains(&"--write-info-json".to_owned()));
        assert!(args.contains(&"youtube:player_client=visionos".to_owned()));
        assert!(!args.contains(&"-".to_owned()), "nothing goes to stdout");
        assert_eq!(
            args.last().unwrap(),
            "https://music.youtube.com/watch?v=abc"
        );
    }

    #[test]
    fn info_path_rejects_odd_ids() {
        let dir = TempDir::new();
        let s = fake_with_info(&dir);
        assert_eq!(s.info_path("a-B_9"), Some(dir.info("a-B_9")));
        assert_eq!(s.info_path("../x"), None);
        assert_eq!(s.info_path(""), None);
        assert_eq!(source("yt-dlp").info_path("abc"), None);
    }

    #[tokio::test]
    async fn open_saves_info_then_uses_it() {
        let dir = TempDir::new();
        let s = fake_with_info(&dir);
        plays(&s, "ok", Duration::ZERO).await;
        assert!(dir.info("ok").exists(), "the first open saves the info");
        assert_eq!(dir.runs("ok"), 1);
        plays(&s, "ok", Duration::from_millis(500)).await;
        assert_eq!(dir.runs("ok"), 1, "the second open extracts nothing");
    }

    #[tokio::test]
    async fn open_uses_a_fresh_file() {
        let dir = TempDir::new();
        std::fs::write(dir.info("ok"), "saved").unwrap();
        plays(&fake_with_info(&dir), "ok", Duration::ZERO).await;
        assert_eq!(std::fs::read_to_string(dir.info("ok")).unwrap(), "saved");
        assert_eq!(dir.runs("ok"), 0);
    }

    #[tokio::test]
    async fn open_ignores_a_stale_file() {
        let dir = TempDir::new();
        std::fs::write(dir.info("ok"), "saved").unwrap();
        age(&dir.info("ok"), INFO_TTL + Duration::from_secs(60));
        plays(&fake_with_info(&dir), "ok", Duration::ZERO).await;
        assert_eq!(dir.runs("ok"), 1, "extracted again");
        assert_eq!(std::fs::read_to_string(dir.info("ok")).unwrap(), "{}\n");
    }

    #[tokio::test]
    async fn failing_load_deletes_the_file_and_falls_back() {
        let dir = TempDir::new();
        std::fs::write(dir.info("ok"), "expired").unwrap();
        plays(&fake_with_info(&dir), "ok", Duration::ZERO).await;
        assert_eq!(dir.runs("ok"), 1, "extracted again");
        assert_eq!(std::fs::read_to_string(dir.info("ok")).unwrap(), "{}\n");
    }

    #[tokio::test]
    async fn prepare_without_info_dir_does_nothing() {
        let s = fake();
        s.prepare("slow");
        assert!(s.in_flight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prepare_saves_info_and_is_deduped() {
        let dir = TempDir::new();
        let s = fake_with_info(&dir);
        s.prepare("slow");
        s.prepare("slow");
        assert_eq!(s.in_flight.lock().unwrap().len(), 1, "in flight once");
        s.wait_for_prepare("slow").await;
        assert!(dir.info("slow").exists());
        assert_eq!(dir.runs("slow"), 1);
        s.prepare("slow");
        assert!(s.in_flight.lock().unwrap().is_empty(), "fresh: skipped");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(dir.runs("slow"), 1);
    }

    #[tokio::test]
    async fn open_waits_for_a_running_prepare() {
        let dir = TempDir::new();
        let s = fake_with_info(&dir);
        s.prepare("slow");
        plays(&s, "slow", Duration::ZERO).await;
        assert_eq!(dir.runs("slow"), 1, "no second extraction");
    }

    #[tokio::test]
    async fn failed_prepare_leaves_the_normal_path() {
        let dir = TempDir::new();
        let s = fake_with_info(&dir);
        s.prepare("bad");
        s.wait_for_prepare("bad").await;
        assert!(!dir.info("bad").exists());
        assert!(s.in_flight.lock().unwrap().is_empty());
    }

    #[test]
    fn sweep_drops_stale_and_keeps_the_newest_64() {
        let dir = TempDir::new();
        for i in 0..70 {
            let path = dir.info(&format!("n{i}"));
            std::fs::write(&path, "x").unwrap();
            age(&path, Duration::from_secs(1000 - i));
        }
        std::fs::write(dir.info("old"), "x").unwrap();
        age(&dir.info("old"), INFO_TTL + Duration::from_secs(60));
        std::fs::write(dir.0.join("other.txt"), "x").unwrap();
        sweep_info_dir(&dir.0, INFO_TTL, 64);
        let mut left: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".info.json"))
            .collect();
        assert_eq!(left.len(), 64);
        assert!(!dir.info("old").exists());
        left.sort();
        assert!(!dir.info("n0").exists(), "the oldest went first");
        assert!(dir.info("n69").exists());
        assert!(dir.0.join("other.txt").exists());
    }
}
