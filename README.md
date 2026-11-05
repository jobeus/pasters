# pasters

A tiny paste.rs-style pastebin for text, markdown, PNGs and JPEGs.

    cargo build --release
    ./target/release/pasters

Pastes are stored as `<id>.txt`, `<id>.png` or `<id>.jpg` in the data dir; the type is detected
from the uploaded bytes. `GET /<id>` and `/<id>.txt` return raw text, `/<id>.md` renders markdown,
and image pastes are served as images at any extension. Other binary uploads get 415. Text over
the size limit is truncated (206); images over theirs are refused (413).

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
deleted (warn), rejected (reason) and rate limited (warn); I/O errors are logged as errors and mark
the span as failed. Example query:

```sql
SELECT start_timestamp, attributes->>'client.address' AS ip, attributes->>'paste.id' AS id,
       attributes->>'paste.kind' AS kind, attributes->>'paste.bytes' AS bytes
FROM records WHERE message LIKE 'paste created%' ORDER BY start_timestamp DESC
```

## Development

    cargo test
    cargo fmt --check && cargo clippy --all-targets -- -D warnings

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs these on pushes and PRs to `main`.
After a green run on `main`, its `deploy` job POSTs `{"sha","ref"}` to `DEPLOY_WEBHOOK_URL`, signed
with HMAC-SHA256 of `<timestamp>.<body>` using `DEPLOY_WEBHOOK_SECRET` (headers `X-Deploy-Timestamp`,
`X-Deploy-Signature: sha256=…`). It is skipped until both repository secrets are set.
