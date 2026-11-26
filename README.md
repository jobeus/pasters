# pasters

A tiny paste.rs-style pastebin for text, markdown, PNGs and JPEGs.

    cargo build --release
    ./target/release/pasters

Pastes are stored as `<id>.txt`, `<id>.png` or `<id>.jpg` in the data dir; the type is detected
from the uploaded bytes. `GET /<id>` and `/<id>.txt` return raw text, `/<id>.md` renders markdown for browsers
(`Accept: text/html`) and returns the raw source to curl/wget,
and image pastes are served as images at any extension. Other binary uploads get 415. Text over
the size limit is truncated (206); images over theirs are refused (413).

In a browser, `/` is an HTML version of the help page: press ⌘V / Ctrl+V anywhere (text or an image)
or drop a file on it to upload it. `/web` has a text form plus an image picker, and also takes
pasted or dropped images. These uploads show a progress bar and then open the new paste. They go
through `POST /` like the API does, so the same limits and rate limit apply.

## Configuration (env vars)

| var | default | |
|---|---|---|
| `PASTE_ADDR` | `127.0.0.1:8000` | listen address (`0.0.0.0:8000` to reach it without a proxy) |
| `PASTE_DATA` | `./data` | where pastes are stored |
| `PASTE_MAX_SIZE` | `131072` | max text paste size in bytes |
| `PASTE_MAX_IMAGE_SIZE` | `20971520` | max image size in bytes |
| `PASTE_RATE_LIMIT` | `10` | pastes per IP per minute (0 = off) |
| `PASTE_TRUST_PROXY` | unset | `1` to take the client IP from `X-Forwarded-For` / `X-Real-IP` |
| `PASTE_BASE_URL` | unset | force the URL in responses, e.g. `https://paste.jobe.wtf` |

Without `PASTE_BASE_URL`, links are built from the request's `Host` (or `X-Forwarded-Host`)
and `X-Forwarded-Proto`, so behind a reverse proxy make sure it forwards those.

### `.env` files

On startup, `.env.<PASTE_ENV>` and then `.env` are loaded from the working directory, if present.
`PASTE_ENV` defaults to `development` for debug builds and `production` for release builds.
Variables already set in the environment win, then `.env.<PASTE_ENV>`, then `.env`. All `.env*`
files except [`.env.example`](.env.example) are git-ignored.

### Observability (Logfire)

| var | default | |
|---|---|---|
| `LOGFIRE_API_KEY` | unset | Logfire write token (`LOGFIRE_TOKEN` also works); unset = console only |
| `RUST_LOG` | `info` | log/trace filter |

Every request becomes a span (`POST / → 201`) with the method, path, status, client IP
(`client.address`, honouring `PASTE_TRUST_PROXY`), peer IP, user agent, referer and body size.
Inside it are events for paste created (id, kind, bytes, truncated, client IP), viewed, not found,
deleted (warn), rejected (reason: `unsupported`, `empty`, `too_large`, `aborted`) and rate limited (warn); I/O errors are logged as errors and mark
the span as failed. `paste.via` (on the created event and the request span) says where a paste came
from: `api`, `web` (the text form), or `<page>-<action>` for browser uploads, e.g. `home-paste` or
`web-upload`. The browser sends that tag in `X-Paste-Client`. Only known values are recorded, and
anything else is logged as `api`. Paste contents, filenames and clipboard data are never logged, and
the browser sends nothing to Logfire directly. Example query:

```sql
SELECT start_timestamp, attributes->>'client.address' AS ip, attributes->>'paste.id' AS id,
       attributes->>'paste.kind' AS kind, attributes->>'paste.bytes' AS bytes
FROM records WHERE message LIKE 'paste created%' ORDER BY start_timestamp DESC
```

## Development

    cargo test
    cargo fmt --check && cargo clippy --all-targets -- -D warnings

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs these on pushes and PRs to `main`.

## Deploy

Deployed on grizzly the standard way (the `grizzly-service` skill; same as musiclinks,
basegame, jobevidz). Push to `main`: CI tests, then POSTs an HMAC-signed request to
`https://paste.jobe.wtf/__deploy` (Actions secrets `DEPLOY_WEBHOOK_URL`,
`DEPLOY_WEBHOOK_SECRET`; the same secret is in the server's `.env`). The hook
([`src/deploy.rs`](src/deploy.rs), 127.0.0.1:8787) runs `git pull --ff-only` and
[`scripts/release-build.sh`](scripts/release-build.sh) while the site keeps serving, then
pasters exits with code 42 and [`start.sh`](start.sh) relaunches it. A failed pull or build
is logged (journal, Logfire) and the old binary keeps running. Webhooks that arrive
mid-build are coalesced into one more pull afterwards.

| var | default | |
|---|---|---|
| `DEPLOY_WEBHOOK_SECRET` | unset | shared HMAC secret; the listener is off when empty |
| `DEPLOY_WEBHOOK_HOST` | `127.0.0.1` | listener interface (Apache proxies to it) |
| `DEPLOY_WEBHOOK_PORT` | `8787` | listener port |
| `DEPLOY_BUILD_COMMAND` | `scripts/release-build.sh` | run after the pull, in the checkout |

On grizzly: checkout `~jobe/webs/paste.jobe.wtf`, unit
[`deploy/pasters.service`](deploy/pasters.service), settings and secrets in `.env` (600:
`DEPLOY_WEBHOOK_SECRET`, `PASTE_TRUST_PROXY=1`, `LOGFIRE_API_KEY`), pastes in `data/`. The
Apache vhost lives in jobepanel (`servers/grizzly/apache/sites-available/jobe.wtf.conf`).
Manual deploy, as jobe in the checkout: `git pull && sudo systemctl restart pasters`.

Logs on grizzly: `journalctl -u pasters -f` (deploys show up as `deploy` lines).
