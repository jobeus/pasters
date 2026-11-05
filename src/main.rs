use std::{
    collections::HashMap,
    env,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Form, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use futures_util::StreamExt;
use pulldown_cmark::{Event, Options, Parser, html};
use rand::RngExt;
use serde::Deserialize;

const ID_LEN: usize = 6;
const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// Bytes needed before we can tell whether an upload is an image.
const SNIFF_LEN: usize = 16;

const INDEX: &str = include_str!("index.txt");
const WEB_FORM: &str = include_str!("web.html");
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

#[tokio::main]
async fn main() {
    let addr: SocketAddr = env::var("PASTE_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8000".into())
        .parse()
        .expect("PASTE_ADDR must be host:port");
    let cfg = Config {
        data_dir: env::var("PASTE_DATA").unwrap_or_else(|_| "data".into()).into(),
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

    let state = Arc::new(AppState { cfg, limiter });
    let app = Router::new()
        .route("/", get(index).post(create))
        .route("/web", get(web_form).post(create_from_form))
        .route("/{name}", get(fetch).delete(remove))
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("listening on http://{addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
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
        .filter(|h| h.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b)))
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

async fn index(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let body = INDEX
        .replace("{URL}", &base_url(&state.cfg, &headers))
        .replace("{MAX_KB}", &(state.cfg.max_size / 1024).to_string())
        .replace("{MAX_IMAGE_MB}", &(state.cfg.max_image_size / (1024 * 1024)).to_string());
    text_response(StatusCode::OK, body)
}

async fn web_form(State(state): State<Shared>) -> Html<String> {
    Html(WEB_FORM.replace("{MAX_KB}", &(state.cfg.max_size / 1024).to_string()))
}

enum PasteError {
    Binary,
    Empty,
    TooLarge,
    RateLimited,
    Io(std::io::Error),
}

impl IntoResponse for PasteError {
    fn into_response(self) -> Response {
        match self {
            PasteError::Binary => text_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported data: only UTF-8 text, markdown, PNG and JPEG are accepted",
            ),
            PasteError::Empty => text_response(StatusCode::BAD_REQUEST, "empty paste"),
            PasteError::TooLarge => text_response(StatusCode::PAYLOAD_TOO_LARGE, "upload too large"),
            PasteError::RateLimited => text_response(
                StatusCode::TOO_MANY_REQUESTS,
                "rate limited, try again in a minute",
            ),
            PasteError::Io(e) => {
                eprintln!("io error: {e}");
                text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

async fn create(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, PasteError> {
    let cfg = &state.cfg;
    if !state.limiter.allow(client_ip(cfg, &headers, peer)) {
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
        let chunk = chunk.map_err(|_| PasteError::Empty)?;
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
        let url = format!("{}/{id}.{}", base_url(cfg, &headers), kind.ext());
        return Ok(text_response(StatusCode::CREATED, url));
    }

    let truncated = total > cfg.max_size;
    let text = decode_text(buf, truncated)?;
    let id = store(cfg, text.as_bytes(), Kind::Text).await?;
    let status = if truncated {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::CREATED
    };
    Ok(text_response(status, format!("{}/{id}", base_url(cfg, &headers))))
}

#[derive(Deserialize)]
struct WebPaste {
    content: String,
}

async fn create_from_form(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<WebPaste>,
) -> Result<Response, PasteError> {
    let cfg = &state.cfg;
    if !state.limiter.allow(client_ip(cfg, &headers, peer)) {
        return Err(PasteError::RateLimited);
    }
    // Browsers submit CRLF line endings from textareas.
    let content = form.content.replace("\r\n", "\n");
    if content.len() > cfg.max_size {
        return Err(PasteError::TooLarge);
    }
    let text = decode_text(content.into_bytes(), false)?;
    let id = store(cfg, text.as_bytes(), Kind::Text).await?;
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
        return text_response(StatusCode::NOT_FOUND, "not found");
    };

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
            let mut resp = (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                text,
            )
                .into_response();
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

async fn remove(State(state): State<Shared>, Path(name): Path<String>) -> Response {
    let Some((id, _)) = parse_name(&name) else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    let Some((_, path)) = find_paste(&state.cfg, id).await else {
        return text_response(StatusCode::NOT_FOUND, "not found");
    };
    match tokio::fs::remove_file(path).await {
        Ok(()) => text_response(StatusCode::OK, "paste deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            text_response(StatusCode::NOT_FOUND, "not found")
        }
        Err(e) => PasteError::Io(e).into_response(),
    }
}
