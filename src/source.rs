//! PCM source: `yt-dlp -o - <url> | ffmpeg … -f s16le -ar 48000 -ac 2 -`.

use std::any::Any;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::task::JoinHandle;

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
}

pub struct YtDlpFfmpegSource {
    pub yt_dlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub cookies_from_browser: Option<String>,
    pub cookies_file: Option<PathBuf>,
    /// YouTube clients yt-dlp asks for the stream (its `player_client`), e.g.
    /// `visionos`; `None` leaves yt-dlp's default. If a song fails with them,
    /// it is opened again with yt-dlp's default.
    pub player_clients: Option<String>,
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
        if let Some(browser) = &self.cookies_from_browser {
            args.extend(["--cookies-from-browser".into(), browser.clone()]);
        }
        if let Some(file) = &self.cookies_file {
            args.extend(["--cookies".into(), file.display().to_string()]);
        }
        args.extend([
            "-o".into(),
            "-".into(),
            format!("https://music.youtube.com/watch?v={video_id}"),
        ]);
        args
    }

    /// Starts `yt-dlp | ffmpeg` and waits for the first PCM bytes.
    pub async fn open_pipeline(
        &self,
        video_id: &str,
        start: Duration,
        player_clients: Option<&str>,
    ) -> anyhow::Result<PipelineReader> {
        let mut yt_dlp = Command::new(&self.yt_dlp)
            .args(self.yt_dlp_args(video_id, player_clients))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", self.yt_dlp.display()))?;
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
        let mut reader = PipelineReader {
            yt_dlp,
            ffmpeg,
            stdout,
            yt_dlp_err,
            ffmpeg_err,
            head: Vec::new(),
            head_pos: 0,
            ended: false,
        };

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
        Ok(Box::new(reader))
    }
}

/// A running `yt-dlp | ffmpeg` pipeline. Dropping it kills both processes.
pub struct PipelineReader {
    yt_dlp: Child,
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
        [self.yt_dlp.id(), self.ffmpeg.id()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Waits for both processes and describes the first one that failed.
    async fn exit_error(&mut self) -> Option<anyhow::Error> {
        let yt_dlp = tokio::time::timeout(EXIT_TIMEOUT, self.yt_dlp.wait()).await;
        let ffmpeg = tokio::time::timeout(EXIT_TIMEOUT, self.ffmpeg.wait()).await;
        let failed = |status: &Result<std::io::Result<std::process::ExitStatus>, _>| !matches!(status, Ok(Ok(s)) if s.success());
        if failed(&yt_dlp) {
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
        YtDlpFfmpegSource {
            yt_dlp: yt_dlp.into(),
            ffmpeg: "ffmpeg".into(),
            cookies_from_browser: None,
            cookies_file: None,
            player_clients: None,
        }
    }

    fn fake() -> YtDlpFfmpegSource {
        source(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-yt-dlp.sh"
        ))
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
}
