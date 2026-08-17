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
mod profile;
mod retrieval;
mod secret;
mod security;
mod source;
mod store;

use std::net::SocketAddr;

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
