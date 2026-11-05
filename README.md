# pasters

A tiny paste.rs-style pastebin for text, markdown, PNGs and JPEGs.

    cargo build --release
    ./target/release/pasters

Pastes are stored as `<id>.txt`, `<id>.png` or `<id>.jpg` in the data dir; the type is detected
from the uploaded bytes. `GET /<id>` and `/<id>.txt` return raw text, `/<id>.md` renders markdown,
and image pastes are served as images at any extension. Other binary uploads get 415. Text over
the size limit is truncated (206); images over theirs are refused (413).

In a browser, `/` is an HTML version of the help page: press ⌘V / Ctrl+V anywhere (text or an image)
or drop a file on it to upload it. `/web` has a text form plus an image picker, and also takes
pasted or dropped images. These uploads show a progress bar and then open the new paste. They go
through `POST /` like the API does, so the same limits and rate limit apply.

## Configuration (env vars)

| var | default | |
|---|---|---|
| `PASTE_ADDR` | `0.0.0.0:8000` | listen address |
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

## Deploying (auto-update on push)

After a green CI run on `main`, the `deploy` job POSTs to the server's deploy webhook, the same
protocol as cmwgpt:

```
POST /deploy
X-Deploy-Timestamp: <unix seconds>
X-Deploy-Signature: sha256=<hex HMAC-SHA256 of "<timestamp>.<body>">
{"sha": "<commit>", "ref": "refs/heads/main"}
```

pasters ([`src/deploy.rs`](src/deploy.rs)) rejects bad signatures or timestamps more than 5 minutes
off (401) and answers `up-to-date` if it is already on that sha. Otherwise it answers 202, then in
the background runs `git pull --ff-only` and `cargo build --release --locked`, and only then exits
with code `42`; [`start.sh`](start.sh) relaunches the new binary. The site keeps serving during the
build, so downtime is just the restart. A failed pull or build is logged (Logfire, journal) and the
old binary keeps running. Webhooks that arrive mid-build are coalesced into one more pull afterwards.

| var | default | |
|---|---|---|
| `DEPLOY_WEBHOOK_SECRET` | unset | shared HMAC secret; the listener is off when empty |
| `DEPLOY_WEBHOOK_HOST` | `127.0.0.1` | listener interface (Apache proxies to it) |
| `DEPLOY_WEBHOOK_PORT` | `8787` | listener port |
| `DEPLOY_BUILD_COMMAND` | `cargo build --release --locked` | run after the pull, in the checkout |

### Server setup (grizzly)

1. In the checkout (e.g. `~/pasters`), put in `.env.production`: `DEPLOY_WEBHOOK_SECRET=<openssl rand -hex 32>`,
   `PASTE_TRUST_PROXY=1` (so real client IPs reach the logs, not Apache's 127.0.0.1) and `LOGFIRE_API_KEY`.
2. Run pasters under systemd via [`deploy/pasters.service`](deploy/pasters.service) (install steps are
   in the file). It runs `start.sh`; stop any other copy on port 8000 first.
3. Use [`deploy/apache-paste.jobe.wtf.conf`](deploy/apache-paste.jobe.wtf.conf) as the vhost: it adds
   `ProxyPass /__deploy` → `127.0.0.1:8787/deploy` ahead of the main proxy, so no extra port is opened.
   Then `sudo apachectl configtest && sudo systemctl reload apache2`.
4. GitHub → Settings → Secrets and variables → Actions: `DEPLOY_WEBHOOK_URL=https://paste.jobe.wtf/__deploy`
   and `DEPLOY_WEBHOOK_SECRET` (same value as on the server).

The checkout must be able to `git pull` non-interactively (deploy key or public HTTPS remote).
Without both Actions secrets the deploy job skips itself. Test by hand:

```bash
body='{"sha":"manual","ref":"refs/heads/main"}'; ts=$(date +%s)
sig=$(printf '%s.%s' "$ts" "$body" | openssl dgst -sha256 -hmac "$DEPLOY_WEBHOOK_SECRET" -r | cut -d' ' -f1)
curl -X POST https://paste.jobe.wtf/__deploy -H "X-Deploy-Timestamp: $ts" -H "X-Deploy-Signature: sha256=$sig" --data "$body"
```
