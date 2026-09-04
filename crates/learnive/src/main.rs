//! learnive — local server for the living document.
//!
//! Topology (§3): a Rust backend as a local HTTP server, rendered in the user's
//! real browser. All filesystem I/O is the backend's; the browser only talks to
//! the backend over localhost. Local-server security lives in `security` (§3.1):
//! bind only to 127.0.0.1, mandatory session token, restricted Origin/Host, no
//! state-changing GET.

mod ai;
mod api;
mod app;
mod config;
mod engine;
mod events;
mod locale;
mod movement;
mod objective;
mod retrieval;
mod secret;
mod security;
mod source;
mod store;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    // Load a local `.env` (if present) before reading any variable. The user's
    // API keys (§12) live here in dev; the file is gitignored.
    load_dotenv(".env");

    // Friendly fixed port by default; overridable via env for dev/tests.
    let port: u16 = std::env::var("LEARNIVE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7420);

    // Single instance per port: the port, not the data dir, is the resource a
    // second launch would actually collide on (`LEARNIVE_DATA_DIR` resolves
    // relative to cwd, so it isn't a stable identity across launches). The
    // lock lives in the system temp dir, keyed by port, so two launches find
    // each other regardless of cwd.
    let lock_path = std::env::temp_dir().join(format!("learnive-{port}.lock"));
    if let Some(lock) = InstanceLock::read(&lock_path)
        && let Some(url) = lock.verify_alive().await
    {
        println!("learnive is already running.");
        println!("Open in your browser: {url}");
        if std::env::var_os("LEARNIVE_NO_OPEN").is_none() {
            open_browser(&url);
        }
        return;
    }

    let state = app::AppState::new(port);
    let router = app::build_router(state.clone());

    // Bind exclusively to 127.0.0.1, never 0.0.0.0 (§3.1).
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not bind http://127.0.0.1:{port}: {e}");
            eprintln!("set LEARNIVE_PORT to choose another port.");
            std::process::exit(1);
        }
    };

    InstanceLock {
        port,
        token: state.token.to_string(),
    }
    .write(&lock_path);

    let url = format!("http://127.0.0.1:{port}/?token={}", state.token);
    println!("learnive running.");
    println!("Open in your browser: {url}");

    // Open the frontend automatically. The listener is already bound and
    // listening, so the browser's connection is accepted into the backlog even
    // before `serve` starts processing — no race. Opt out with LEARNIVE_NO_OPEN.
    if std::env::var_os("LEARNIVE_NO_OPEN").is_none() {
        open_browser(&url);
    }

    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("server exited with error: {e}");
        std::process::exit(1);
    }
}

/// Which port/token a running instance is bound to. Written (owner-only,
/// `0600`) after a successful bind at `$TMPDIR/learnive-{port}.lock`, read
/// before the next launch on that same port tries to bind — a crash can leave
/// a stale file behind, so it is only ever trusted after
/// [`InstanceLock::verify_alive`] confirms it over HTTP.
#[derive(Serialize, Deserialize)]
struct InstanceLock {
    port: u16,
    token: String,
}

impl InstanceLock {
    fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write(&self, path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let Ok(json) = serde_json::to_vec(self) else {
            return;
        };
        // Owner-only permissions (0600): this file carries the session token
        // that gates access to the user's stored API keys (§3.1), same
        // convention as `secret::set_owner_only`. Written before the rename
        // so the file is never briefly world-readable.
        let tmp = path.with_extension("lock.tmp");
        if std::fs::write(&tmp, &json).is_err() {
            return;
        }
        let _ = secret::set_owner_only(&tmp);
        let _ = std::fs::rename(&tmp, path);
    }

    /// Confirms a live server actually answers at `port` with this exact
    /// token — the file alone isn't enough, since it may be stale after a
    /// crash or the port may since have been recycled by an unrelated
    /// process. Returns the confirmed instance's URL.
    async fn verify_alive(&self) -> Option<String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let resp = client
            .get(format!("http://127.0.0.1:{}/health", self.port))
            .header("x-learnive-token", &self.token)
            .send()
            .await
            .ok()?;
        resp.status()
            .is_success()
            .then(|| format!("http://127.0.0.1:{}/?token={}", self.port, self.token))
    }
}

/// Opens `url` in the user's default browser. Dependency-free per platform:
/// `xdg-open` on Linux/BSD, `open` on macOS, `cmd /C start` on Windows. Spawned
/// and not awaited; failure is non-fatal (the URL is already printed as a
/// fallback). The token is in the URL so the opened tab is authenticated (§3.1).
fn open_browser(url: &str) {
    use std::process::{Command, Stdio};

    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // The first quoted `start` argument is the window title, so pass an empty one.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    // Detach stdio so the browser launcher never noises up our console.
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if spawned.is_err() {
        eprintln!("could not open the browser automatically — open the URL above manually.");
    }
}

/// Loads variables from a `.env` file (`KEY=VALUE` per line) into the process
/// environment. Deliberately minimal (no dependency): ignores blank lines and
/// comments (`#`), strips surrounding single/double quotes from the value, and
/// **does not override** variables already present — the real environment wins.
/// A missing file is silent (demo mode runs without a key).
fn load_dotenv(path: &str) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Tolerate an optional leading `export `.
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        // SAFETY: single-threaded startup, before any tokio thread.
        unsafe { std::env::set_var(key, value) };
    }
}
