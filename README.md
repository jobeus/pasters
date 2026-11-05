# pasters

A tiny paste.rs-style pastebin for text and markdown.

    cargo build --release
    ./target/release/pasters

Pastes are stored as `<id>.txt` in the data dir. `GET /<id>` and `/<id>.txt` return raw text,
`/<id>.md` renders markdown. Binary uploads get 415; uploads over the size limit are truncated (206).

## Configuration (env vars)

| var | default | |
|---|---|---|
| `PASTE_ADDR` | `0.0.0.0:8000` | listen address |
| `PASTE_DATA` | `./data` | where pastes are stored |
| `PASTE_MAX_SIZE` | `131072` | max paste size in bytes |
| `PASTE_RATE_LIMIT` | `10` | pastes per IP per minute (0 = off) |
| `PASTE_TRUST_PROXY` | unset | `1` to take the client IP from `X-Forwarded-For` / `X-Real-IP` |
| `PASTE_BASE_URL` | unset | force the URL in responses, e.g. `https://paste.jobe.wtf` |

Without `PASTE_BASE_URL`, links are built from the request's `Host` (or `X-Forwarded-Host`)
and `X-Forwarded-Proto`, so behind a reverse proxy make sure it forwards those.
