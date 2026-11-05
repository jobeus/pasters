//! Deploy webhook: CI POSTs here after tests pass on `main`; we `git pull`, rebuild, and
//! exit with [`RESTART_EXIT_CODE`] so `start.sh` relaunches the new binary.
//!
//! Request format (`POST /deploy`), the same as cmwgpt's:
//!
//! ```text
//! X-Deploy-Timestamp: <unix seconds>
//! X-Deploy-Signature: sha256=<hex HMAC-SHA256 of "<timestamp>.<raw body>" keyed by the secret>
//! {"sha": "<commit sha>", "ref": "refs/heads/main"}
//! ```
//!
//! The pull and build happen *before* exiting, so the paste server keeps serving the old
//! binary during the (multi-minute) release build and is only down for the restart itself.
//! A failed pull or build is logged and the old binary keeps running.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use tokio::{process::Command, sync::watch};

/// `start.sh` relaunches the server when it exits with this code.
pub const RESTART_EXIT_CODE: i32 = 42;
/// Requests whose timestamp is further than this from now are rejected (replay guard).
const MAX_CLOCK_SKEW_SECS: u64 = 300;

/// Why the server is stopping, broadcast to both listeners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Running,
    /// Ctrl-C / SIGTERM: exit normally.
    Signal,
    /// A deploy finished building: exit with [`RESTART_EXIT_CODE`].
    Restart,
}

pub struct DeployConfig {
    pub secret: String,
    /// The git checkout to pull and build in.
    pub repo_dir: PathBuf,
    /// Run through `sh -c` in `repo_dir` after pulling.
    pub build_command: String,
}

pub struct Deployer {
    cfg: DeployConfig,
    stop: watch::Sender<Stop>,
    /// `Some(rerun)` while a deploy runs; `rerun` is set when another webhook arrives
    /// meanwhile, so a push that lands mid-build still gets pulled afterwards.
    in_progress: Mutex<Option<bool>>,
}

impl Deployer {
    pub fn new(cfg: DeployConfig, stop: watch::Sender<Stop>) -> Arc<Self> {
        Arc::new(Deployer {
            cfg,
            stop,
            in_progress: Mutex::new(None),
        })
    }
}

pub fn router(deployer: Arc<Deployer>) -> Router {
    Router::new()
        .route("/deploy", post(handle_deploy))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(deployer)
}

/// HMAC-SHA256 over `"<timestamp>.<body>"`.
fn mac(secret: &str, timestamp: &str, body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// The `X-Deploy-Signature` value CI sends.
#[cfg(test)]
fn sign(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let digest = mac(secret, timestamp, body).finalize().into_bytes();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

fn verify(secret: &str, headers: &HeaderMap, body: &[u8], now: u64) -> bool {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let (Some(timestamp), Some(signature)) = (get("x-deploy-timestamp"), get("x-deploy-signature"))
    else {
        return false;
    };
    let Ok(sent_at) = timestamp.parse::<u64>() else {
        return false;
    };
    if now.abs_diff(sent_at) > MAX_CLOCK_SKEW_SECS {
        return false;
    }
    let Some(hex) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Some(sig) = decode_hex(hex) else {
        return false;
    };
    // Constant-time comparison.
    mac(secret, timestamp, body).verify_slice(&sig).is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Deserialize)]
struct DeployRequest {
    #[serde(default)]
    sha: String,
}

async fn handle_deploy(
    State(d): State<Arc<Deployer>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !verify(&d.cfg.secret, &headers, &body, unix_now()) {
        tracing::warn!("deploy webhook rejected: bad signature or timestamp");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    let Ok(req) = serde_json::from_slice::<DeployRequest>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid body"})),
        )
            .into_response();
    };
    let sha = req.sha;

    let current = git_head(&d.cfg).await;
    if !sha.is_empty() && current.as_deref() == Some(sha.as_str()) {
        tracing::info!(deploy.sha = %sha, "deploy webhook: already running {sha}");
        return Json(json!({"status": "up-to-date", "sha": sha})).into_response();
    }

    {
        let mut in_progress = d.in_progress.lock().unwrap();
        if let Some(rerun) = in_progress.as_mut() {
            *rerun = true;
            tracing::info!(deploy.sha = %sha, "deploy webhook: deploy already running, queued");
            return (
                StatusCode::ACCEPTED,
                Json(json!({"status": "queued", "sha": sha})),
            )
                .into_response();
        }
        *in_progress = Some(false);
    }
    tracing::info!(
        deploy.sha = %sha,
        deploy.current = current.as_deref().unwrap_or(""),
        "deploy webhook: deploying {sha}"
    );
    tokio::spawn(run_deploys(d));
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "deploying", "sha": sha})),
    )
        .into_response()
}

/// Pulls and builds until no webhook arrived meanwhile, then asks the server to restart.
async fn run_deploys(d: Arc<Deployer>) {
    loop {
        let result = pull_and_build(&d.cfg).await;
        let rerun = {
            let mut in_progress = d.in_progress.lock().unwrap();
            let rerun = in_progress.take() == Some(true);
            if rerun {
                *in_progress = Some(false);
            }
            rerun
        };
        if rerun {
            continue;
        }
        match result {
            Ok(head) => {
                tracing::info!(deploy.sha = %head, "deploy built {head}, restarting");
                d.stop.send_replace(Stop::Restart);
            }
            Err(e) => tracing::error!(error = %e, "deploy failed; still running the old build"),
        }
        return;
    }
}

async fn pull_and_build(cfg: &DeployConfig) -> Result<String, String> {
    run(cfg, "git", &["pull", "--ff-only"]).await?;
    run(cfg, "sh", &["-c", &cfg.build_command]).await?;
    git_head(cfg)
        .await
        .ok_or_else(|| "git rev-parse HEAD failed".into())
}

async fn git_head(cfg: &DeployConfig) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cfg.repo_dir)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn run(cfg: &DeployConfig, program: &str, args: &[&str]) -> Result<(), String> {
    let cmd = format!("{program} {}", args.join(" "));
    let out = Command::new(program)
        .args(args)
        .current_dir(&cfg.repo_dir)
        .output()
        .await
        .map_err(|e| format!("{cmd}: {e}"))?;
    let tail = |b: &[u8]| {
        let s = String::from_utf8_lossy(b);
        let s = s.trim();
        s[s.len().saturating_sub(2000)..].to_owned()
    };
    if out.status.success() {
        tracing::info!(command = %cmd, output = %tail(&out.stderr), "deploy step ok: {cmd}");
        Ok(())
    } else {
        Err(format!(
            "{cmd} failed ({}): {}",
            out.status,
            tail(&out.stderr)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use std::{path::Path, time::Duration};
    use tower::ServiceExt;

    const SECRET: &str = "s3cret";

    fn signed(body: &str, ts: u64, secret: &str) -> Request<Body> {
        let ts = ts.to_string();
        Request::post("/deploy")
            .header("x-deploy-timestamp", &ts)
            .header("x-deploy-signature", sign(secret, &ts, body.as_bytes()))
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[test]
    fn signature_matches_ci_openssl_format() {
        // From the CI job's own command:
        //   printf '%s.%s' 1700000000 '{"sha":"abc"}' | openssl dgst -sha256 -hmac s3cret -r
        assert_eq!(
            sign(SECRET, "1700000000", br#"{"sha":"abc"}"#),
            "sha256=20adb4e0e3984d95bf2cdb0968052cc482d23bda359606901edb0bcaea68e8af"
        );
    }

    #[test]
    fn verify_checks_signature_and_clock_skew() {
        let body = br#"{"sha":"abc"}"#;
        let now = 1_700_000_000;
        let headers = |ts: u64, sig: String| {
            let mut h = HeaderMap::new();
            h.insert("x-deploy-timestamp", ts.to_string().parse().unwrap());
            h.insert("x-deploy-signature", sig.parse().unwrap());
            h
        };
        let good = |ts: u64| headers(ts, sign(SECRET, &ts.to_string(), body));
        assert!(verify(SECRET, &good(now), body, now));
        assert!(verify(SECRET, &good(now - 299), body, now));
        assert!(!verify(SECRET, &good(now - 301), body, now));
        assert!(!verify(SECRET, &good(now + 301), body, now));
        assert!(!verify(SECRET, &good(now), b"{\"sha\":\"evil\"}", now));
        assert!(!verify("other", &good(now), body, now));
        assert!(!verify(
            SECRET,
            &headers(now, "sha256=zz".into()),
            body,
            now
        ));
        assert!(!verify(SECRET, &headers(now, "abc".into()), body, now));
        assert!(!verify(SECRET, &HeaderMap::new(), body, now));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// An upstream repo and a clone of it standing in for the server's checkout.
    fn repos() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "-q"]);
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(tmp.path(), &["clone", "-q", "upstream", "checkout"]);
        (tmp, upstream, checkout)
    }

    fn deployer(checkout: PathBuf, build_command: &str) -> (Arc<Deployer>, watch::Receiver<Stop>) {
        let (tx, rx) = watch::channel(Stop::Running);
        let cfg = DeployConfig {
            secret: SECRET.into(),
            repo_dir: checkout,
            build_command: build_command.into(),
        };
        (Deployer::new(cfg, tx), rx)
    }

    async fn send(d: &Arc<Deployer>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = router(d.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn rejects_unsigned_and_answers_up_to_date() {
        let (_tmp, _upstream, checkout) = repos();
        let head = git(&checkout, &["rev-parse", "HEAD"]);
        let (d, rx) = deployer(checkout, "true");

        let body = format!(r#"{{"sha":"{head}"}}"#);
        let (status, _) = send(&d, signed(&body, unix_now(), "wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(&d, signed("not json", unix_now(), SECRET)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, json) = send(&d, signed(&body, unix_now(), SECRET)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"], "up-to-date");
        assert_eq!(*rx.borrow(), Stop::Running);
    }

    #[tokio::test]
    async fn new_commit_is_pulled_built_and_restarts() {
        let (_tmp, upstream, checkout) = repos();
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let new_head = git(&upstream, &["rev-parse", "HEAD"]);
        // The build step runs in the checkout after the pull.
        let (d, mut rx) = deployer(checkout.clone(), "git log -1 --format=%s > built.txt");

        let body = format!(r#"{{"sha":"{new_head}"}}"#);
        let (status, json) = send(&d, signed(&body, unix_now(), SECRET)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(json["status"], "deploying");

        tokio::time::timeout(
            Duration::from_secs(10),
            rx.wait_for(|s| *s == Stop::Restart),
        )
        .await
        .expect("no restart")
        .unwrap();
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), new_head);
        assert_eq!(
            std::fs::read_to_string(checkout.join("built.txt"))
                .unwrap()
                .trim(),
            "two"
        );
    }

    #[tokio::test]
    async fn failed_build_keeps_running() {
        let (_tmp, upstream, checkout) = repos();
        git(&upstream, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let (d, rx) = deployer(checkout, "exit 1");

        let (status, _) = send(&d, signed(r#"{"sha":"x"}"#, unix_now(), SECRET)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        // Wait for the background deploy to finish.
        for _ in 0..100 {
            if d.in_progress.lock().unwrap().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(d.in_progress.lock().unwrap().is_none());
        assert_eq!(*rx.borrow(), Stop::Running);
    }
}
