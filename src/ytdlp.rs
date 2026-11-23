//! The yt-dlp binary Pinson runs: swappable at run time, with an updater that keeps
//! its own copy (a Python venv) so YouTube fixes land without rebuilding the image.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use tokio::process::Command;

/// Longest `<tool> --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a whole update may take.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long a looked-up yt-dlp version is trusted.
const VERSION_TTL: Duration = Duration::from_secs(10 * 60);
/// How long the latest PyPI version is kept.
const LATEST_TTL: Duration = Duration::from_secs(6 * 3600);
/// How long a failed PyPI lookup is kept.
const LATEST_FAIL_TTL: Duration = Duration::from_secs(10 * 60);
/// Log lines kept when an update fails.
const LOG_TAIL: usize = 20;
/// Where the latest yt-dlp version is looked up.
pub const PYPI_URL: &str = "https://pypi.org";

/// The yt-dlp path every spawn uses. Clones share it, so a switch reaches all of them.
#[derive(Clone, Debug)]
pub struct YtDlpBin(Arc<RwLock<PathBuf>>);

impl YtDlpBin {
    /// The binary to run now.
    pub fn get(&self) -> PathBuf {
        self.0.read().unwrap().clone()
    }

    /// Makes later spawns run `path`; a process already running keeps its binary.
    pub fn set(&self, path: PathBuf) {
        *self.0.write().unwrap() = path;
    }
}

impl From<PathBuf> for YtDlpBin {
    fn from(path: PathBuf) -> Self {
        Self(Arc::new(RwLock::new(path)))
    }
}

impl From<&str> for YtDlpBin {
    fn from(path: &str) -> Self {
        PathBuf::from(path).into()
    }
}

/// First line of `<bin> <arg>` (stdout), trimmed; `None` if it can't run, fails or times out.
pub async fn tool_version(bin: &Path, arg: &str) -> Option<String> {
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        Command::new(bin)
            .arg(arg)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    // `ffmpeg version 9.0.2 Copyright (c) …`: the copyright is noise.
    let line = line.split(" Copyright").next().unwrap_or(line).trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// The version `bin --version` prints.
pub async fn version(bin: &Path) -> Option<String> {
    tool_version(bin, "--version").await
}

/// True when `candidate` is a later yt-dlp version than `than` (dot-separated numbers,
/// so `2026.10.26` < `2026.10.26.1` < `2026.11.2`). Anything non-numeric is never newer.
pub fn newer(candidate: &str, than: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        v.trim().split('.').map(|p| p.parse().ok()).collect()
    }
    match (parts(candidate), parts(than)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Reads `info.version` from PyPI's JSON for a project.
pub fn parse_pypi(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let version = value.get("info")?.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// How an update stands. Serialized as `{"state": "idle" | "running" | "done" | "failed", ...}`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UpdateState {
    Idle,
    Running {
        started_at: String,
    },
    Done {
        version: String,
        at: String,
        /// Whether Pinson now runs the updated copy.
        switched: bool,
    },
    Failed {
        error: String,
        at: String,
        /// The last lines of the venv and pip output.
        log: Vec<String>,
    },
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The latest yt-dlp version on PyPI, looked up lazily and cached.
struct Latest {
    base: Option<String>,
    client: reqwest::Client,
    cache: tokio::sync::Mutex<Option<(Instant, Option<String>)>>,
}

impl Latest {
    fn new(base: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "Pinson/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/Gekkotron/Pinson)"
            ))
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build http client");
        Self {
            base,
            client,
            cache: Default::default(),
        }
    }

    async fn get(&self) -> Option<String> {
        let base = self.base.as_deref()?;
        let mut cache = self.cache.lock().await;
        if let Some((at, value)) = &*cache {
            let ttl = if value.is_some() {
                LATEST_TTL
            } else {
                LATEST_FAIL_TTL
            };
            if at.elapsed() < ttl {
                return value.clone();
            }
        }
        let value = match self.fetch(base).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("latest yt-dlp lookup failed: {e:#}");
                None
            }
        };
        *cache = Some((Instant::now(), value.clone()));
        value
    }

    async fn fetch(&self, base: &str) -> anyhow::Result<String> {
        let url = format!("{}/pypi/yt-dlp/json", base.trim_end_matches('/'));
        let body = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_pypi(&body).ok_or_else(|| anyhow::anyhow!("no version in the PyPI answer"))
    }
}

struct Inner {
    bin: YtDlpBin,
    configured: PathBuf,
    dir: PathBuf,
    python: PathBuf,
    latest: Latest,
    state: Mutex<UpdateState>,
    version: Mutex<Option<(Instant, Option<String>)>>,
}

/// The yt-dlp in use plus its updater. Cheap to clone.
#[derive(Clone)]
pub struct YtDlp {
    inner: Arc<Inner>,
}

/// What `GET /api/health` shows about yt-dlp, minus the update state.
pub struct YtDlpInfo {
    pub path: PathBuf,
    pub version: Option<String>,
    pub latest: Option<String>,
    pub managed: bool,
}

/// The directory for Pinson's own yt-dlp: `yt-dlp` next to the database.
pub fn managed_dir(db: &Path) -> PathBuf {
    match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join("yt-dlp"),
        _ => PathBuf::from(".").join("yt-dlp"),
    }
}

impl YtDlp {
    /// `bin` is what spawns read; `configured` is the binary it starts as; `dir` holds the
    /// managed venv; `python` creates it; `pypi` is the PyPI base URL (`None` skips the lookup).
    pub fn new(
        bin: YtDlpBin,
        configured: PathBuf,
        dir: PathBuf,
        python: PathBuf,
        pypi: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                bin,
                configured,
                dir,
                python,
                latest: Latest::new(pypi),
                state: Mutex::new(UpdateState::Idle),
                version: Mutex::default(),
            }),
        }
    }

    fn managed_bin(&self) -> PathBuf {
        self.inner.dir.join("bin").join("yt-dlp")
    }

    /// The binary new songs use.
    pub fn bin(&self) -> YtDlpBin {
        self.inner.bin.clone()
    }

    /// The update state now.
    pub fn state(&self) -> UpdateState {
        self.inner.state.lock().unwrap().clone()
    }

    /// Picks the managed copy if it is strictly newer than the configured binary. Runs in
    /// the background; until it ends the configured binary is used.
    pub fn start(&self) {
        let this = self.clone();
        tokio::spawn(async move { this.choose().await });
    }

    async fn choose(&self) {
        let configured = version(&self.inner.configured).await;
        let managed_bin = self.managed_bin();
        let managed = if managed_bin.exists() {
            version(&managed_bin).await
        } else {
            None
        };
        let use_managed = match (&managed, &configured) {
            (Some(m), Some(c)) => newer(m, c),
            (Some(_), None) => true,
            _ => false,
        };
        let (path, version) = if use_managed {
            self.inner.bin.set(managed_bin.clone());
            (managed_bin, managed)
        } else {
            (self.inner.configured.clone(), configured)
        };
        *self.inner.version.lock().unwrap() = Some((Instant::now(), version.clone()));
        tracing::info!(
            "yt-dlp: {} ({})",
            version.as_deref().unwrap_or("not found"),
            path.display()
        );
    }

    /// The version of the binary in use (cached; looked up again after a switch).
    pub async fn current_version(&self) -> Option<String> {
        if let Some((at, v)) = &*self.inner.version.lock().unwrap()
            && at.elapsed() < VERSION_TTL
        {
            return v.clone();
        }
        let v = version(&self.inner.bin.get()).await;
        *self.inner.version.lock().unwrap() = Some((Instant::now(), v.clone()));
        v
    }

    /// Path, versions and whether the managed copy is in use.
    pub async fn info(&self) -> YtDlpInfo {
        let path = self.inner.bin.get();
        let (version, latest) = tokio::join!(self.current_version(), self.inner.latest.get());
        YtDlpInfo {
            managed: path == self.managed_bin(),
            path,
            version,
            latest,
        }
    }

    /// Starts an update in the background and returns the running state; `Err` holds the
    /// running state when one is already going.
    pub fn start_update(&self) -> Result<UpdateState, UpdateState> {
        let running = {
            let mut state = self.inner.state.lock().unwrap();
            if matches!(*state, UpdateState::Running { .. }) {
                return Err(state.clone());
            }
            let running = UpdateState::Running { started_at: now() };
            *state = running.clone();
            running
        };
        let this = self.clone();
        tokio::spawn(async move {
            let log = Arc::new(Mutex::new(Vec::new()));
            let result = match tokio::time::timeout(UPDATE_TIMEOUT, this.run_update(&log)).await {
                Ok(r) => r,
                Err(_) => Err(format!(
                    "timed out after {} minutes",
                    UPDATE_TIMEOUT.as_secs() / 60
                )),
            };
            let state = match result {
                Ok((version, switched)) => {
                    tracing::info!("yt-dlp updated to {version} (in use: {switched})");
                    UpdateState::Done {
                        version,
                        at: now(),
                        switched,
                    }
                }
                Err(error) => {
                    tracing::warn!("yt-dlp update failed: {error}");
                    let log = log.lock().unwrap();
                    UpdateState::Failed {
                        error,
                        at: now(),
                        log: log[log.len().saturating_sub(LOG_TAIL)..].to_vec(),
                    }
                }
            };
            *this.inner.state.lock().unwrap() = state;
        });
        Ok(running)
    }

    /// Installs or upgrades yt-dlp in the venv; returns its version and whether Pinson
    /// switched to it.
    async fn run_update(&self, log: &Arc<Mutex<Vec<String>>>) -> Result<(String, bool), String> {
        let dir = self.inner.dir.display().to_string();
        let managed_bin = self.managed_bin();
        if !self.inner.dir.join("bin").join("pip").exists() {
            let ok = self
                .run(&self.inner.python, &["-m", "venv", &dir], log)
                .await?;
            if !ok {
                return Err(format!("creating the venv failed: {}", last_line(log)));
            }
        }
        let pip = self.inner.dir.join("bin").join("pip");
        let ok = self
            .run(
                &pip,
                &[
                    "install",
                    "--upgrade",
                    "--no-cache-dir",
                    "--disable-pip-version-check",
                    "yt-dlp[default]",
                ],
                log,
            )
            .await?;
        if !ok {
            return Err(format!("pip install failed: {}", last_line(log)));
        }
        let Some(new) = version(&managed_bin).await else {
            return Err("yt-dlp does not run after the install".to_owned());
        };
        let configured = version(&self.inner.configured).await;
        let switched = configured.as_deref().is_none_or(|c| !newer(c, &new));
        if switched {
            self.inner.bin.set(managed_bin);
        } else {
            self.inner.bin.set(self.inner.configured.clone());
        }
        *self.inner.version.lock().unwrap() = None;
        Ok((new, switched))
    }

    /// Runs one command, adding its output lines to `log`; `Ok(false)` if it exits non-zero.
    async fn run(
        &self,
        program: &Path,
        args: &[&str],
        log: &Arc<Mutex<Vec<String>>>,
    ) -> Result<bool, String> {
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| {
                if program == self.inner.python && e.kind() == std::io::ErrorKind::NotFound {
                    "python3 not found: install Python 3 to update yt-dlp from Pinson".to_owned()
                } else {
                    format!("starting {}: {e}", program.display())
                }
            })?;
        let mut log = log.lock().unwrap();
        for stream in [&output.stdout, &output.stderr] {
            for line in String::from_utf8_lossy(stream).lines() {
                if !line.trim().is_empty() {
                    log.push(line.trim_end().to_owned());
                }
            }
        }
        Ok(output.status.success())
    }
}

fn last_line(log: &Arc<Mutex<Vec<String>>>) -> String {
    log.lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_else(|| "no output".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numeric_parts() {
        assert!(newer("2026.10.26.1", "2026.10.26"));
        assert!(newer("2026.11.2", "2026.10.26.1"));
        assert!(newer("2026.11.10", "2026.11.2"));
        assert!(!newer("2026.10.26", "2026.10.26"));
        assert!(!newer("2026.10.26", "2026.10.26.1"));
        assert!(!newer("2026.10.26", "2026.11.2"));
    }

    #[test]
    fn newer_is_false_for_non_numeric() {
        assert!(!newer("nightly", "2026.10.26"));
        assert!(!newer("2026.11.2", "unknown"));
        assert!(!newer("", "2026.10.26"));
    }

    #[test]
    fn parses_the_pypi_answer() {
        assert_eq!(
            parse_pypi(r#"{"info":{"version":"2026.11.20","name":"yt-dlp"},"urls":[]}"#),
            Some("2026.11.20".to_owned())
        );
        assert_eq!(parse_pypi(r#"{"info":{}}"#), None);
        assert_eq!(parse_pypi("not json"), None);
    }

    #[test]
    fn managed_dir_sits_next_to_the_database() {
        assert_eq!(
            managed_dir(Path::new("/data/pinson.db")),
            PathBuf::from("/data/yt-dlp")
        );
        assert_eq!(
            managed_dir(Path::new("pinson.db")),
            PathBuf::from("./yt-dlp")
        );
    }

    #[test]
    fn update_state_serializes_camel_case() {
        let v = serde_json::to_value(UpdateState::Idle).unwrap();
        assert_eq!(v, serde_json::json!({"state": "idle"}));
        let v = serde_json::to_value(UpdateState::Running {
            started_at: "t".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({"state": "running", "startedAt": "t"}));
    }

    #[test]
    fn bin_clones_share_the_path() {
        let a = YtDlpBin::from("one");
        let b = a.clone();
        b.set(PathBuf::from("two"));
        assert_eq!(a.get(), PathBuf::from("two"));
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn script(path: &Path, body: &str) {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// A fake python3 whose `-m venv DIR` makes a pip that installs a yt-dlp printing
        /// `version`.
        fn fake_python(dir: &Path, version: &str, pip_exit: i32) -> PathBuf {
            let python = dir.join("python3");
            script(
                &python,
                &format!(
                    r#"mkdir -p "$3/bin"
cat > "$3/bin/pip" <<'PIP'
#!/bin/sh
echo "Collecting yt-dlp"
if [ {pip_exit} -ne 0 ]; then echo "ERROR: no network" >&2; exit {pip_exit}; fi
D=$(dirname "$0")
printf '#!/bin/sh\necho {version}\n' > "$D/yt-dlp"
chmod +x "$D/yt-dlp"
PIP
chmod +x "$3/bin/pip"
echo "venv made""#
                ),
            );
            python
        }

        fn ytdlp(root: &Path, configured: &str, python: PathBuf) -> YtDlp {
            let configured_bin = root.join("configured");
            script(&configured_bin, &format!("echo {configured}"));
            YtDlp::new(
                YtDlpBin::from(configured_bin.clone()),
                configured_bin,
                root.join("managed"),
                python,
                None,
            )
        }

        async fn finished(y: &YtDlp) -> UpdateState {
            for _ in 0..200 {
                let s = y.state();
                if !matches!(s, UpdateState::Running { .. }) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("update never ended");
        }

        fn temp(name: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("pinson-ytdlp-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[tokio::test]
        async fn update_switches_to_a_newer_copy() {
            let root = temp("newer");
            let y = ytdlp(&root, "2026.01.01", fake_python(&root, "2026.11.20", 0));
            assert!(y.start_update().is_ok());
            assert!(y.start_update().is_err(), "second update while running");
            match finished(&y).await {
                UpdateState::Done {
                    version, switched, ..
                } => {
                    assert_eq!(version, "2026.11.20");
                    assert!(switched);
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(y.bin().get(), root.join("managed/bin/yt-dlp"));
            let info = y.info().await;
            assert!(info.managed);
            assert_eq!(info.version.as_deref(), Some("2026.11.20"));
            let _ = std::fs::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn update_keeps_a_newer_configured_binary() {
            let root = temp("older");
            let y = ytdlp(&root, "2027.01.01", fake_python(&root, "2026.11.20", 0));
            y.start_update().unwrap();
            match finished(&y).await {
                UpdateState::Done {
                    version, switched, ..
                } => {
                    assert_eq!(version, "2026.11.20");
                    assert!(!switched);
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(y.bin().get(), root.join("configured"));
            let _ = std::fs::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn failed_pip_reports_the_log_tail() {
            let root = temp("pipfail");
            let y = ytdlp(&root, "2026.01.01", fake_python(&root, "2026.11.20", 1));
            y.start_update().unwrap();
            match finished(&y).await {
                UpdateState::Failed { error, log, .. } => {
                    assert!(error.contains("pip install failed"), "{error}");
                    assert!(error.contains("no network"), "{error}");
                    assert!(log.iter().any(|l| l == "Collecting yt-dlp"));
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(y.bin().get(), root.join("configured"));
            // A failed update can be retried.
            assert!(y.start_update().is_ok());
            let _ = std::fs::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn missing_python_is_explained() {
            let root = temp("nopython");
            let y = ytdlp(&root, "2026.01.01", root.join("no-such-python"));
            y.start_update().unwrap();
            match finished(&y).await {
                UpdateState::Failed { error, .. } => {
                    assert!(error.starts_with("python3 not found"), "{error}");
                }
                other => panic!("{other:?}"),
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn start_prefers_a_strictly_newer_managed_copy() {
            let root = temp("start");
            let y = ytdlp(&root, "2026.01.01", root.join("unused"));
            std::fs::create_dir_all(root.join("managed/bin")).unwrap();
            script(&root.join("managed/bin/yt-dlp"), "echo 2026.05.05");
            y.choose().await;
            assert_eq!(y.bin().get(), root.join("managed/bin/yt-dlp"));

            let root2 = temp("start-same");
            let y2 = ytdlp(&root2, "2026.05.05", root2.join("unused"));
            std::fs::create_dir_all(root2.join("managed/bin")).unwrap();
            script(&root2.join("managed/bin/yt-dlp"), "echo 2026.05.05");
            y2.choose().await;
            assert_eq!(y2.bin().get(), root2.join("configured"));
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&root2);
        }

        #[tokio::test]
        async fn latest_is_read_from_pypi_and_cached() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = hits.clone();
            let app = axum::Router::new().route(
                "/pypi/yt-dlp/json",
                axum::routing::get(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async { r#"{"info":{"version":"2026.12.1"}}"# }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let latest = Latest::new(Some(base));
            assert_eq!(latest.get().await.as_deref(), Some("2026.12.1"));
            assert_eq!(latest.get().await.as_deref(), Some("2026.12.1"));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
    }
}
