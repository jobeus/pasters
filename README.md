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
