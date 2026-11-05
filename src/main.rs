use std::{
    collections::HashMap,
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Extension, Form, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use futures_util::StreamExt;
use pulldown_cmark::{Event, Options, Parser, html};
use rand::RngExt;
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{Instrument, field};

use deploy::Stop;

mod deploy;

const ID_LEN: usize = 6;
const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// Bytes needed before we can tell whether an upload is an image.
const SNIFF_LEN: usize = 16;

const INDEX: &str = include_str!("index.txt");
const INDEX_PAGE: &str = include_str!("index.html");
const WEB_FORM: &str = include_str!("web.html");
const UPLOAD_SCRIPT: &str = include_str!("upload.js");
const UPLOAD_STYLE: &str = include_str!("upload.css");
/// CSP for our own HTML pages: inline script and style only, XHR back to us, nothing else.
const PAGE_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
     connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
/// `X-Paste-Client` values sent by the upload script (`<page>-<action>`). Only these are logged
/// as `paste.via`, so a client can't put arbitrary strings into the logs.
const WEB_CLIENTS: [&str; 6] = [
    "home-paste",
    "home-drop",
    "home-upload",
    "web-paste",
    "web-drop",
    "web-upload",
];
const MD_PAGE: &str = include_str!("md.html");

struct Config {
    data_dir: PathBuf,
    base_url: Option<String>,
    max_size: usize,
    max_image_size: usize,
    trust_proxy: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Png,
    Jpeg,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Text, Kind::Png, Kind::Jpeg];

    fn ext(self) -> &'static str {
        match self {
            Kind::Text => "txt",
            Kind::Png => "png",
            Kind::Jpeg => "jpg",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Kind::Text => "text/plain; charset=utf-8",
            Kind::Png => "image/png",
            Kind::Jpeg => "image/jpeg",
        }
    }
}

struct AppState {
    cfg: Config,
    limiter: RateLimiter,
}

type Shared = Arc<AppState>;

/// Fixed-window per-IP limiter for paste creation.
struct RateLimiter {
    per_min: u32,
    hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    fn allow(&self, ip: IpAddr) -> bool {
        if self.per_min == 0 {
            return true;
        }
        let window = Duration::from_secs(60);
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap();
        if hits.len() > 10_000 {
            hits.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let entry = hits.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        if entry.1 >= self.per_min {
            return false;
        }
        entry.1 += 1;
        true
    }
}

/// The client address resolved by [`observe`], available to handlers as an extension.
#[derive(Clone, Copy)]
struct ClientIp(IpAddr);

/// `PASTE_ENV` if set, otherwise `development` for debug builds and `production` for release.
fn app_env() -> String {
    env::var("PASTE_ENV")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(debug_assertions) {
                "development"
            } else {
                "production"
            }
            .into()
        })
}

/// Loads `.env.<mode>` then `.env` from `dir`. Variables already set win over both, and
/// `.env.<mode>` wins over `.env`, since dotenvy never overwrites. Returns the files loaded.
fn load_env(dir: &FsPath, mode: &str) -> Vec<PathBuf> {
    [format!(".env.{mode}"), ".env".into()]
        .into_iter()
        .map(|name| dir.join(name))
        .filter(|path| match dotenvy::from_path(path) {
            Ok(()) => true,
            Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => panic!("failed to load {}: {e}", path.display()),
        })
        .collect()
}

/// Sends traces and logs to Logfire when `LOGFIRE_API_KEY` (or `LOGFIRE_TOKEN`) is set,
/// and always prints them to the console.
fn init_logfire(mode: &str) -> logfire::Logfire {
    let mut builder = logfire::configure()
        .send_to_logfire(logfire::config::SendToLogfire::IfTokenPresent)
        .with_service_name("pasters")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_environment(mode)
        .with_console(Some(logfire::config::ConsoleOptions::default()))
        // Logfire would default to TRACE, which includes hyper's and the exporter's own
        // internals. `RUST_LOG` still overrides this.
        .with_default_level_filter(tracing::level_filters::LevelFilter::INFO);
    if let Some(key) = env::var("LOGFIRE_API_KEY").ok().filter(|k| !k.is_empty()) {
        builder = builder.with_token(key);
    }
    builder.finish().expect("failed to configure logfire")
}

#[tokio::main]
async fn main() {
    let mode = app_env();
    let env_files = load_env(FsPath::new("."), &mode);
    let logfire = init_logfire(&mode);
    let logfire_enabled = env::var("LOGFIRE_API_KEY").is_ok_and(|k| !k.is_empty())
        || env::var("LOGFIRE_TOKEN").is_ok();

    let addr: SocketAddr = env::var("PASTE_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8000".into())
        .parse()
        .expect("PASTE_ADDR must be host:port");
    let cfg = Config {
        data_dir: env::var("PASTE_DATA")
            .unwrap_or_else(|_| "data".into())
            .into(),
        base_url: env::var("PASTE_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('/').to_string()),
        max_size: env_num("PASTE_MAX_SIZE", 128 * 1024),
        max_image_size: env_num("PASTE_MAX_IMAGE_SIZE", 20 * 1024 * 1024),
        trust_proxy: env::var("PASTE_TRUST_PROXY").is_ok_and(|v| v == "1" || v == "true"),
    };
    let limiter = RateLimiter {
        per_min: env_num("PASTE_RATE_LIMIT", 10),
        hits: Mutex::new(HashMap::new()),
    };
    tokio::fs::create_dir_all(&cfg.data_dir)
        .await
        .expect("failed to create data dir");

    let app = app(Arc::new(AppState { cfg, limiter }));

    let (stop_tx, stop_rx) = watch::channel(Stop::Running);
    {
        let stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            stop_tx.send_replace(Stop::Signal);
        });
    }
    let deploy_server = spawn_deploy_listener(stop_tx, stop_rx.clone()).await;

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!(
        %addr,
        env = %mode,
        env_files = ?env_files,
        logfire = logfire_enabled,
        deploy_webhook = deploy_server.is_some(),
        "listening on http://{addr}"
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stopped(stop_rx.clone()))
    .await
    .unwrap();
    if let Some(server) = deploy_server {
        // Lets the deploy listener finish sending its response.
        let _ = server.await;
    }

    let restart = *stop_rx.borrow() == Stop::Restart;
    tracing::info!(restart, "shutting down");
    // Flush buffered spans and logs before exiting.
    if let Err(e) = logfire.shutdown() {
        eprintln!("logfire shutdown failed: {e}");
    }
    if restart {
        std::process::exit(deploy::RESTART_EXIT_CODE);
    }
}

/// Resolves once anything asks the server to stop.
async fn stopped(mut rx: watch::Receiver<Stop>) {
    let _ = rx.wait_for(|s| *s != Stop::Running).await;
}

/// Starts the deploy webhook listener when `DEPLOY_WEBHOOK_SECRET` is set.
async fn spawn_deploy_listener(
    stop_tx: watch::Sender<Stop>,
    stop_rx: watch::Receiver<Stop>,
) -> Option<tokio::task::JoinHandle<()>> {
    let secret = env::var("DEPLOY_WEBHOOK_SECRET").unwrap_or_default();
    if secret.is_empty() {
        tracing::info!("deploy webhook disabled (DEPLOY_WEBHOOK_SECRET not set)");
        return None;
    }
    let addr = format!(
        "{}:{}",
        env::var("DEPLOY_WEBHOOK_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        env::var("DEPLOY_WEBHOOK_PORT").unwrap_or_else(|_| "8787".into()),
    );
    let deployer = deploy::Deployer::new(
        deploy::DeployConfig {
            secret,
            repo_dir: env::current_dir().expect("no working directory"),
            build_command: env::var("DEPLOY_BUILD_COMMAND")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "cargo build --release --locked".into()),
        },
        stop_tx,
    );
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "deploy webhook could not bind {addr}");
            return None;
        }
    };
    tracing::info!(%addr, "deploy webhook listening on {addr}/deploy");
    Some(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, deploy::router(deployer))
            .with_graceful_shutdown(stopped(stop_rx))
            .await
        {
            tracing::error!(error = %e, "deploy webhook server failed");
        }
    }))
}

fn app(state: Shared) -> Router {
    Router::new()
        .route("/", get(index).post(create))
        .route("/web", get(web_form).post(create_from_form))
        .route("/{name}", get(fetch).delete(remove))
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), observe))
        .with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok() };
    #[cfg(unix)]
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<Option<()>>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// Wraps every request in a span carrying the method, path, client IP, user agent and
/// response status, and hands the resolved client IP to handlers.
async fn observe(State(state): State<Shared>, mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), |c| c.0);
    let ip = client_ip(&state.cfg, req.headers(), peer);
    req.extensions_mut().insert(ClientIp(ip));

    // Scoped so no borrow of `req` (whose body isn't `Sync`) lives across the await below.
    let (user_agent, referer, body_size) = {
        let header = |name: header::HeaderName| -> String {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        (
            header(header::USER_AGENT),
            header(header::REFERER),
            header(header::CONTENT_LENGTH),
        )
    };
    let method = req.method().to_string();
    let path = req.uri().path().to_owned();
    let span = tracing::info_span!(
        "HTTP request",
        logfire.msg = format!("{method} {path}"),
        http.request.method = method.as_str(),
        url.path = path.as_str(),
        client.address = ip.to_string(),
        network.peer.address = peer.ip().to_string(),
        user_agent.original = user_agent,
        http.request.header.referer = referer,
        http.request.body.size = body_size,
        paste.via = field::Empty,
        http.response.status_code = field::Empty,
        otel.status_code = field::Empty,
    );
    let resp = next.run(req).instrument(span.clone()).await;
    let status = resp.status().as_u16();
    span.record("http.response.status_code", status);
    span.record("logfire.msg", format!("{method} {path} → {status}"));
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
    resp
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The externally visible origin, e.g. `https://paste.jobe.wtf`.
fn base_url(cfg: &Config, headers: &HeaderMap) -> String {
    if let Some(url) = &cfg.base_url {
        return url.clone();
    }
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let proto = get("x-forwarded-proto")
        .and_then(|p| p.split(',').next())
        .map(str::trim)
        .filter(|p| *p == "http" || *p == "https")
        .unwrap_or("http");
    let host = get("x-forwarded-host")
        .or_else(|| get("host"))
        .and_then(|h| h.split(',').next())
        .map(str::trim)
        .filter(|h| {
            h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b))
        })
        .unwrap_or("localhost");
    format!("{proto}://{host}")
}

fn client_ip(cfg: &Config, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if cfg.trust_proxy {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        // The rightmost X-Forwarded-For entry is the one our proxy appended.
        let forwarded = get("x-forwarded-for")
            .and_then(|v| v.rsplit(',').next())
            .or_else(|| get("x-real-ip"))
            .and_then(|v| v.trim().parse().ok());
        if let Some(ip) = forwarded {
            return ip;
        }
    }
    peer.ip()
}

fn text_response(status: StatusCode, body: impl Into<String>) -> Response {
    let mut body = body.into();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

fn fill_limits(template: &str, cfg: &Config) -> String {
    template
        .replace("{MAX_KB}", &(cfg.max_size / 1024).to_string())
        .replace(
            "{MAX_IMAGE_MB}",
            &(cfg.max_image_size / (1024 * 1024)).to_string(),
        )
        .replace("{MAX_TEXT}", &cfg.max_size.to_string())
        .replace("{MAX_IMAGE}", &cfg.max_image_size.to_string())
}

/// An HTML page with the upload script, served with [`PAGE_CSP`].
fn page_response(template: &str, cfg: &Config, extra: &[(&str, &str)]) -> Response {
    let mut page = fill_limits(template, cfg);
    for (key, value) in extra {
        page = page.replace(key, value);
    }
    // Last, so nothing inside the script or stylesheet is mistaken for a placeholder.
    let page = page
        .replace("{UPLOAD_STYLE}", UPLOAD_STYLE)
        .replace("{UPLOAD_SCRIPT}", UPLOAD_SCRIPT);
    let mut resp = Html(page).into_response();
    resp.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PAGE_CSP),
    );
    resp
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Plain text for curl and friends; browsers get the same text in an HTML page that also
/// accepts pasted and dropped content.
async fn index(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let url = base_url(&state.cfg, &headers);
    let text = fill_limits(INDEX, &state.cfg).replace("{URL}", &url);
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    let mut resp = if wants_html {
        let web = escape_html(&format!("{url}/web"));
        let body = escape_html(&text).replacen(&web, &format!("<a href=\"/web\">{web}</a>"), 1);
        page_response(INDEX_PAGE, &state.cfg, &[("{INDEX}", &body)])
    } else {
        text_response(StatusCode::OK, text)
    };
    resp.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    resp
}

async fn web_form(State(state): State<Shared>) -> Response {
    page_response(WEB_FORM, &state.cfg, &[])
}

enum PasteError {
    /// The client went away mid-upload, after sending this many bytes.
    Aborted(usize),
    Binary,
    Empty,
    TooLarge,
    RateLimited,
    Io(std::io::Error),
}

impl IntoResponse for PasteError {
    fn into_response(self) -> Response {
        match &self {
            PasteError::RateLimited => tracing::warn!("paste rejected: rate limited"),
            PasteError::Aborted(received) => tracing::info!(
                reason = "aborted",
                paste.bytes_received = *received as u64,
                "paste rejected"
            ),
            PasteError::Io(e) => tracing::error!(error = %e, "io error"),
            PasteError::Binary => tracing::info!(reason = "unsupported", "paste rejected"),
            PasteError::Empty => tracing::info!(reason = "empty", "paste rejected"),
            PasteError::TooLarge => tracing::info!(reason = "too_large", "paste rejected"),
        }
        match self {
            PasteError::Aborted(_) => text_response(StatusCode::BAD_REQUEST, "upload aborted"),
            PasteError::Binary => text_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported data: only UTF-8 text, markdown, PNG and JPEG are accepted",
            ),
            PasteError::Empty => text_response(StatusCode::BAD_REQUEST, "empty paste"),
            PasteError::TooLarge => {
                text_response(StatusCode::PAYLOAD_TOO_LARGE, "upload too large")
            }
            PasteError::RateLimited => text_response(
                StatusCode::TOO_MANY_REQUESTS,
                "rate limited, try again in a minute",
            ),
            PasteError::Io(_) => text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        }
    }
}

/// Records a stored paste; `client.address` is repeated here (not just on the request span)
/// so uploads can be queried by IP directly.
fn log_created(id: &str, kind: Kind, bytes: usize, truncated: bool, ip: IpAddr, via: &str) {
    tracing::info!(
        paste.id = id,
        paste.kind = kind.ext(),
        paste.bytes = bytes as u64,
        paste.truncated = truncated,
        paste.via = via,
        client.address = %ip,
        "paste created {id}"
    );
}

async fn create(
    State(state): State<Shared>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, PasteError> {
    let cfg = &state.cfg;
    let via = headers
        .get("x-paste-client")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| WEB_CLIENTS.iter().find(|c| **c == v).copied())
        .unwrap_or("api");
    tracing::Span::current().record("paste.via", via);
    if !state.limiter.allow(ip) {
        return Err(PasteError::RateLimited);
    }

    // Once the first bytes say whether this is an image, keep only as much as
    // that kind may store. Text past its limit is drained (but discarded) so the
    // client gets a clean response, up to a hard ceiling.
    let hard_limit = cfg.max_image_size.max(cfg.max_size);
    let mut buf = Vec::new();
    let mut cap: Option<usize> = None;
    let mut total = 0usize;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| PasteError::Aborted(total))?;
        total += chunk.len();
        if total > hard_limit {
            return Err(PasteError::TooLarge);
        }
        let room = cap.unwrap_or(usize::MAX).saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..room.min(chunk.len())]);
        if cap.is_none() && buf.len() >= SNIFF_LEN {
            let limit = match sniff_image(&buf) {
                Some(_) => cfg.max_image_size,
                None => cfg.max_size,
            };
            buf.truncate(limit);
            cap = Some(limit);
        }
    }

    if let Some(kind) = sniff_image(&buf) {
        // A truncated image is useless, so oversized ones are refused instead.
        if total > cfg.max_image_size {
            return Err(PasteError::TooLarge);
        }
        if !image_complete(kind, &buf) {
            return Err(PasteError::Binary);
        }
        let id = store(cfg, &buf, kind).await?;
        log_created(&id, kind, buf.len(), false, ip, via);
        let url = format!("{}/{id}.{}", base_url(cfg, &headers), kind.ext());
        return Ok(text_response(StatusCode::CREATED, url));
    }

    let truncated = total > cfg.max_size;
    let text = decode_text(buf, truncated)?;
    let id = store(cfg, text.as_bytes(), Kind::Text).await?;
    log_created(&id, Kind::Text, text.len(), truncated, ip, via);
    let status = if truncated {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::CREATED
    };
    Ok(text_response(
        status,
        format!("{}/{id}", base_url(cfg, &headers)),
    ))
}

#[derive(Deserialize)]
struct WebPaste {
    content: String,
}

async fn create_from_form(
    State(state): State<Shared>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Form(form): Form<WebPaste>,
) -> Result<Response, PasteError> {
    let cfg = &state.cfg;
    tracing::Span::current().record("paste.via", "web");
    if !state.limiter.allow(ip) {
        return Err(PasteError::RateLimited);
    }
    // Browsers submit CRLF line endings from textareas.
    let content = form.content.replace("\r\n", "\n");
    if content.len() > cfg.max_size {
        return Err(PasteError::TooLarge);
    }
    let text = decode_text(content.into_bytes(), false)?;
    let id = store(cfg, text.as_bytes(), Kind::Text).await?;
    log_created(&id, Kind::Text, text.len(), false, ip, "web");
    Ok(Redirect::to(&format!("/{id}")).into_response())
}

/// Identifies PNG and JPEG uploads by their magic bytes.
fn sniff_image(buf: &[u8]) -> Option<Kind> {
    if buf.starts_with(b"\x89PNG\r\n\x1a\n") && buf.get(12..16) == Some(b"IHDR") {
        Some(Kind::Png)
    } else if buf.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Kind::Jpeg)
    } else {
        None
    }
}

/// Cheap sanity check that the whole image arrived. JPEGs often carry trailing
/// data after the end-of-image marker (e.g. motion photos), so only PNGs are checked.
fn image_complete(kind: Kind, buf: &[u8]) -> bool {
    match kind {
        Kind::Png => buf.ends_with(b"IEND\xaeB`\x82"),
        _ => true,
    }
}

/// Accepts only UTF-8 text; anything that looks binary is rejected.
/// When the upload was truncated, a multi-byte character cut at the end is dropped.
fn decode_text(buf: Vec<u8>, truncated: bool) -> Result<String, PasteError> {
    if buf.contains(&0) {
        return Err(PasteError::Binary);
    }
    let text = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) if truncated && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).map_err(|_| PasteError::Binary)?
        }
        Err(_) => return Err(PasteError::Binary),
    };
    if text.trim().is_empty() {
        return Err(PasteError::Empty);
    }
    // Text may contain the odd control char (ANSI escapes, form feeds), but
    // lots of them means it's binary that happens to be valid UTF-8.
    let controls = text
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\x0c' | '\x1b'))
        .count();
    if controls * 100 > text.chars().count() {
        return Err(PasteError::Binary);
    }
    Ok(text)
}

async fn store(cfg: &Config, data: &[u8], kind: Kind) -> Result<String, PasteError> {
    use tokio::io::AsyncWriteExt;
    loop {
        let id: String = {
            let mut rng = rand::rng();
            (0..ID_LEN)
                .map(|_| ID_CHARS[rng.random_range(0..ID_CHARS.len())] as char)
                .collect()
        };
        // Ids are shared across kinds, so `abc123.txt` and `abc123.png` can't coexist.
        if find_paste(cfg, &id).await.is_some() {
            continue;
        }
        let path = cfg.data_dir.join(format!("{id}.{}", kind.ext()));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                file.write_all(data).await.map_err(PasteError::Io)?;
                return Ok(id);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(PasteError::Io(e)),
        }
    }
}

/// Splits `abc123.md` into (`abc123`, Some("md")), rejecting anything that
/// isn't a well-formed id so it can never escape the data dir.
fn parse_name(name: &str) -> Option<(&str, Option<&str>)> {
    let (id, ext) = match name.split_once('.') {
        Some((id, ext)) => (id, Some(ext)),
        None => (name, None),
    };
    let valid = !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then_some((id, ext))
}

async fn find_paste(cfg: &Config, id: &str) -> Option<(Kind, PathBuf)> {
    for kind in Kind::ALL {
        let path = cfg.data_dir.join(format!("{id}.{}", kind.ext()));
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Some((kind, path));
        }
    }
    None
}

async fn fetch(State(state): State<Shared>, Path(name): Path<String>) -> Response {
    let Some((id, ext)) = parse_name(&name) else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let Some((kind, path)) = find_paste(&state.cfg, id).await else {
        tracing::info!(paste.id = id, "paste not found");
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    tracing::info!(
        paste.id = id,
        paste.kind = kind.ext(),
        paste.ext = ext.unwrap_or(""),
        "paste viewed {id}"
    );

    // Images are served as themselves whatever extension was asked for.
    if !matches!(kind, Kind::Text) {
        let Ok(data) = tokio::fs::read(&path).await else {
            return text_response(StatusCode::NOT_FOUND, "not found");
        };
        return (
            [
                (header::CONTENT_TYPE, kind.content_type()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
            ],
            data,
        )
            .into_response();
    }
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };

    match ext.map(str::to_ascii_lowercase).as_deref() {
        Some("md" | "mdown" | "markdown") => {
            let page = MD_PAGE
                .replace("{TITLE}", id)
                .replace("{BODY}", &render_markdown(&text));
            let mut resp = Html(page).into_response();
            // Raw HTML in pastes is escaped, but CSP also blocks scripts
            // (including javascript: links) as a second line of defence.
            resp.headers_mut().insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; style-src 'unsafe-inline'; img-src * data:; base-uri 'none'; form-action 'none'",
                ),
            );
            resp
        }
        _ => {
            let mut resp =
                ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response();
            resp.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            resp
        }
    }
}

fn render_markdown(src: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    // Show raw HTML as text instead of injecting it into the page.
    let events = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        ev => ev,
    });
    let mut out = String::with_capacity(src.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

async fn remove(
    State(state): State<Shared>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(name): Path<String>,
) -> Response {
    let Some((id, _)) = parse_name(&name) else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let Some((kind, path)) = find_paste(&state.cfg, id).await else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    match tokio::fs::remove_file(path).await {
        Ok(()) => {
            tracing::warn!(
                paste.id = id,
                paste.kind = kind.ext(),
                client.address = %ip,
                "paste deleted {id}"
            );
            text_response(StatusCode::OK, "paste deleted")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            text_response(StatusCode::NOT_FOUND, "not found")
        }
        Err(e) => PasteError::Io(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::connect_info::MockConnectInfo, http::Request as HttpRequest};
    use tower::ServiceExt;

    const PEER: ([u8; 4], u16) = ([10, 0, 0, 1], 4242);

    struct TestApp {
        app: Router,
        dir: tempfile::TempDir,
    }

    fn test_config(data_dir: PathBuf) -> Config {
        Config {
            data_dir,
            base_url: Some("https://paste.test".into()),
            max_size: 64,
            max_image_size: 256,
            trust_proxy: false,
        }
    }

    fn test_app(edit: impl FnOnce(&mut Config, &mut u32)) -> TestApp {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(dir.path().to_path_buf());
        let mut per_min = 0;
        edit(&mut cfg, &mut per_min);
        let limiter = RateLimiter {
            per_min,
            hits: Mutex::new(HashMap::new()),
        };
        let app =
            app(Arc::new(AppState { cfg, limiter })).layer(MockConnectInfo(SocketAddr::from(PEER)));
        TestApp { app, dir }
    }

    impl TestApp {
        async fn send(&self, req: HttpRequest<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
            let resp = self.app.clone().oneshot(req).await.unwrap();
            let (parts, body) = resp.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            (parts.status, parts.headers, body.to_vec())
        }

        async fn post(&self, body: impl Into<Body>) -> (StatusCode, String) {
            let req = HttpRequest::post("/").body(body.into()).unwrap();
            let (status, _, body) = self.send(req).await;
            (status, String::from_utf8(body).unwrap())
        }

        async fn get(&self, path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
            self.send(HttpRequest::get(path).body(Body::empty()).unwrap())
                .await
        }

        async fn delete(&self, path: &str) -> StatusCode {
            self.send(HttpRequest::delete(path).body(Body::empty()).unwrap())
                .await
                .0
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<_> = std::fs::read_dir(self.dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    /// Path part of a URL returned by the server, e.g. `/abc123.png`.
    fn path_of(url: &str) -> &str {
        url.trim()
            .strip_prefix("https://paste.test")
            .expect("unexpected base url")
    }

    fn png(len: usize) -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        data.resize(len - 8, b'x');
        data.extend_from_slice(b"IEND\xaeB`\x82");
        data
    }

    #[tokio::test]
    async fn text_paste_round_trip() {
        let t = test_app(|_, _| {});
        let (status, url) = t.post("hello world").await;
        assert_eq!(status, StatusCode::CREATED);
        let path = path_of(&url);
        assert_eq!(path.len(), 1 + ID_LEN);

        let (status, headers, body) = t.get(path).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(body, b"hello world");
        // Any non-markdown extension is raw text too.
        assert_eq!(t.get(&format!("{path}.txt")).await.2, b"hello world");
        assert_eq!(t.files(), vec![format!("{}.txt", &path[1..])]);
    }

    #[tokio::test]
    async fn markdown_is_rendered_with_html_escaped() {
        let t = test_app(|_, _| {});
        let (_, url) = t.post("# Title\n\n<script>alert(1)</script>").await;
        let (status, headers, body) = t.get(&format!("{}.md", path_of(&url))).await;
        assert_eq!(status, StatusCode::OK);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("<h1>Title</h1>"), "{body}");
        assert!(!body.contains("<script>alert"), "{body}");
        assert!(body.contains("&lt;script&gt;"), "{body}");
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.starts_with("default-src 'none'"));
    }

    #[tokio::test]
    async fn oversized_text_is_truncated() {
        let t = test_app(|_, _| {});
        let (status, url) = t.post("a".repeat(100)).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(t.get(path_of(&url)).await.2, b"a".repeat(64));
    }

    #[tokio::test]
    async fn truncation_drops_a_split_multibyte_char() {
        let t = test_app(|_, _| {});
        // 63 ASCII bytes then a 3-byte char straddling the 64-byte limit.
        let (status, url) = t.post(format!("{}€tail", "a".repeat(63))).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(t.get(path_of(&url)).await.2, b"a".repeat(63));
    }

    #[tokio::test]
    async fn empty_and_binary_pastes_are_rejected() {
        let t = test_app(|_, _| {});
        assert_eq!(t.post("").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(t.post("   \n\t").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            t.post(vec![0u8, 1, 2, 3]).await.0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            t.post(vec![0xffu8, 0xfe, 0x41]).await.0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            t.post("\x01\x02\x03\x04 lots of control").await.0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert!(t.files().is_empty());
    }

    #[tokio::test]
    async fn png_is_detected_and_served_as_image() {
        let t = test_app(|_, _| {});
        let data = png(100);
        let (status, url) = t.post(data.clone()).await;
        assert_eq!(status, StatusCode::CREATED);
        let path = path_of(&url);
        assert!(path.ends_with(".png"), "{url}");

        // Served as PNG whatever extension is requested.
        let bare = path.trim_end_matches(".png");
        for p in [path, bare, &format!("{bare}.md")] {
            let (status, headers, body) = t.get(p).await;
            assert_eq!(status, StatusCode::OK, "{p}");
            assert_eq!(headers[header::CONTENT_TYPE], "image/png");
            assert_eq!(body, data);
        }
    }

    #[tokio::test]
    async fn jpeg_is_detected() {
        let t = test_app(|_, _| {});
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xE0];
        data.resize(80, 0);
        let (status, url) = t.post(data).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(url.trim().ends_with(".jpg"), "{url}");
        assert_eq!(
            t.get(path_of(&url)).await.1[header::CONTENT_TYPE],
            "image/jpeg"
        );
    }

    #[tokio::test]
    async fn incomplete_or_oversized_png_is_refused() {
        let t = test_app(|_, _| {});
        let mut cut = png(100);
        cut.truncate(90);
        assert_eq!(t.post(cut).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(t.post(png(300)).await.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(t.files().is_empty());
    }

    #[tokio::test]
    async fn web_form_creates_paste_and_normalises_crlf() {
        let t = test_app(|_, _| {});
        let req = HttpRequest::post("/web")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("content=line1%0D%0Aline2"))
            .unwrap();
        let (status, headers, _) = t.send(req).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = headers[header::LOCATION].to_str().unwrap().to_owned();
        assert_eq!(t.get(&location).await.2, b"line1\nline2");
    }

    #[tokio::test]
    async fn delete_removes_paste() {
        let t = test_app(|_, _| {});
        let (_, url) = t.post("bye").await;
        let path = path_of(&url);
        assert_eq!(t.delete(path).await, StatusCode::OK);
        assert_eq!(t.get(path).await.0, StatusCode::NOT_FOUND);
        assert_eq!(t.delete(path).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_and_malformed_names_are_404() {
        let t = test_app(|_, _| {});
        for path in ["/nope12", "/..%2Fetc", "/a-b", "/x/y"] {
            assert_eq!(t.get(path).await.0, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn index_lists_limits_and_url() {
        let t = test_app(|_, _| {});
        let (status, _, body) = t.get("/").await;
        assert_eq!(status, StatusCode::OK);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("https://paste.test"), "{body}");
        assert!(
            !body.contains("{URL}") && !body.contains("{MAX_KB}"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn browsers_get_an_html_homepage_with_the_upload_script() {
        let t = test_app(|_, _| {});
        let req = HttpRequest::get("/")
            .header(header::ACCEPT, "text/html,application/xhtml+xml,*/*;q=0.8")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = t.send(req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(headers[header::VARY], "Accept");
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("connect-src 'self'"), "{csp}");
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains(r#"data-page="home""#), "{body}");
        assert!(body.contains(r#"data-max-text="64""#), "{body}");
        assert!(body.contains(r#"data-max-image="256""#), "{body}");
        assert!(body.contains("X-Paste-Client"), "script missing");
        // The index text is escaped, and the web form URL is a link.
        assert!(body.contains("GET https://paste.test/&lt;id&gt;"), "{body}");
        assert!(body.contains(r#"<a href="/web">https://paste.test/web</a>"#));
        assert!(
            !body.contains("{UPLOAD_") && !body.contains("{MAX_"),
            "{body}"
        );

        let (_, headers, body) = t.get("/web").await;
        let body = String::from_utf8(body).unwrap();
        assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY));
        assert!(body.contains(r#"data-page="web""#) && body.contains(r#"id="file""#));
        assert!(
            !body.contains("{UPLOAD_") && !body.contains("{MAX_"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn uploads_from_the_page_script_work_like_api_uploads() {
        let t = test_app(|_, _| {});
        // Known and unknown client tags are both accepted; only the tag in the logs differs.
        for client in ["home-paste", "web-upload", "<script>"] {
            let req = HttpRequest::post("/")
                .header("x-paste-client", client)
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png(100)))
                .unwrap();
            let (status, _, body) = t.send(req).await;
            assert_eq!(status, StatusCode::CREATED, "{client}");
            assert!(String::from_utf8(body).unwrap().trim().ends_with(".png"));
        }
    }

    #[tokio::test]
    async fn rate_limit_is_per_client_ip() {
        let t = test_app(|cfg, per_min| {
            cfg.trust_proxy = true;
            *per_min = 2;
        });
        let post_as = |ip: &'static str| {
            HttpRequest::post("/")
                .header("x-forwarded-for", ip)
                .body(Body::from("x"))
                .unwrap()
        };
        assert_eq!(t.send(post_as("1.1.1.1")).await.0, StatusCode::CREATED);
        assert_eq!(t.send(post_as("1.1.1.1")).await.0, StatusCode::CREATED);
        assert_eq!(
            t.send(post_as("1.1.1.1")).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(t.send(post_as("2.2.2.2")).await.0, StatusCode::CREATED);
    }

    #[test]
    fn client_ip_only_trusts_proxy_headers_when_configured() {
        let peer = SocketAddr::from(PEER);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "6.6.6.6, 9.9.9.9".parse().unwrap());
        let mut cfg = test_config(PathBuf::new());
        assert_eq!(client_ip(&cfg, &headers, peer), peer.ip());
        cfg.trust_proxy = true;
        // The rightmost entry is the one our proxy appended; the rest is client-supplied.
        assert_eq!(
            client_ip(&cfg, &headers, peer),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
        headers.clear();
        headers.insert("x-real-ip", "7.7.7.7".parse().unwrap());
        assert_eq!(
            client_ip(&cfg, &headers, peer),
            "7.7.7.7".parse::<IpAddr>().unwrap()
        );
        headers.insert("x-real-ip", "garbage".parse().unwrap());
        assert_eq!(client_ip(&cfg, &headers, peer), peer.ip());
    }

    #[test]
    fn base_url_prefers_config_then_forwarded_headers() {
        let mut cfg = test_config(PathBuf::new());
        let mut headers = HeaderMap::new();
        headers.insert("host", "internal:8000".parse().unwrap());
        assert_eq!(base_url(&cfg, &headers), "https://paste.test");
        cfg.base_url = None;
        assert_eq!(base_url(&cfg, &headers), "http://internal:8000");
        headers.insert("x-forwarded-host", "paste.example.com".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(base_url(&cfg, &headers), "https://paste.example.com");
        headers.insert("x-forwarded-host", "evil.com/<script>".parse().unwrap());
        headers.insert("x-forwarded-proto", "javascript".parse().unwrap());
        assert_eq!(base_url(&cfg, &headers), "http://localhost");
    }

    #[test]
    fn parse_name_rejects_anything_but_alphanumeric_ids() {
        assert_eq!(parse_name("abc123"), Some(("abc123", None)));
        assert_eq!(parse_name("abc123.md"), Some(("abc123", Some("md"))));
        assert_eq!(parse_name("a.b.c"), Some(("a", Some("b.c"))));
        for bad in ["", ".md", "..", "../x", "a/b", "a-b", &"a".repeat(33)] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn load_env_prefers_existing_vars_then_mode_file_then_dotenv() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "PASTERS_T_BASE=base\nPASTERS_T_BOTH=base\nPASTERS_T_SET=base\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".env.staging"),
            "PASTERS_T_BOTH=staging\nPASTERS_T_SET=staging\n",
        )
        .unwrap();
        // SAFETY: these variable names are unique to this test.
        unsafe { env::set_var("PASTERS_T_SET", "process") };

        let loaded = load_env(dir.path(), "staging");
        assert_eq!(
            loaded,
            vec![dir.path().join(".env.staging"), dir.path().join(".env")]
        );
        assert_eq!(env::var("PASTERS_T_BASE").unwrap(), "base");
        assert_eq!(env::var("PASTERS_T_BOTH").unwrap(), "staging");
        assert_eq!(env::var("PASTERS_T_SET").unwrap(), "process");

        // Missing files are skipped, not errors.
        assert!(load_env(dir.path(), "nonexistent").ends_with(&[dir.path().join(".env")]));
        assert!(load_env(tempfile::tempdir().unwrap().path(), "production").is_empty());
    }
}
