# Porting nginx-rtmp-module to Rust

Source: fork `jose-sanchez100/nginx-rtmp-module` @ `6c7719d` (identical to upstream
`arut/nginx-rtmp-module` v1.2.x). A local reference copy lives in `c-reference/`
(ignored by git).

## Architecture decision

The original is an **nginx module** (C, ~26k lines, built on nginx's event loop and pools).
The port is a **standalone RTMP server** on `tokio`:

- Same protocol behaviour (handshake, chunking, commands, status messages, `live`
  semantics), verified against ffmpeg.
- Same configuration syntax: it reads the `rtmp { server { application ... } }` block
  from an existing `nginx.conf`. Directives that are not ported yet are logged and ignored.
- No need to build nginx (the `ngx-rust` bindings are impractical on Windows and would tie
  the port to a specific nginx version).
- One task per connection plus a writer task with a bounded queue (`out_queue`); the
  priority-based frame dropping (disposable → inter → key) mirrors `ngx_rtmp_send_message`.

## Status per C file

| C | Rust | Status |
|---|---|---|
| `ngx_rtmp_amf.c` | `src/amf.rs` | ✅ full AMF0 (incl. Envivio's unterminated arrays) |
| `ngx_rtmp_handshake.c` | `src/handshake.rs` | ✅ server + client, simple and digest (FP9) |
| `ngx_rtmp_handler.c` | `src/chunk.rs`, `src/session.rs` | ✅ chunks, ack, ping, aggregate |
| `ngx_rtmp_receive.c` | `src/session.rs` | ✅ protocol control and user control messages |
| `ngx_rtmp_send.c` | `src/message.rs` | ✅ |
| `ngx_rtmp_init.c`, `ngx_rtmp.c` | `src/server.rs`, `src/session.rs` | ✅ (no `proxy_protocol`) |
| `ngx_rtmp_core_module.c` | `src/config.rs` | ✅ core directives (`so_keepalive` not applied yet) |
| `ngx_rtmp_cmd_module.c` | `src/session.rs` | ✅ connect/createStream/publish/play/play2/pause/close/delete |
| `ngx_rtmp_live_module.c` | `src/live.rs` | ✅ wait_key, wait_video, interleave, sync, publish_notify, play_restart, idle_streams, drop_idle_publisher |
| `ngx_rtmp_codec_module.c` | `src/codec.rs` | 🟡 AAC/AVC sequence headers, `meta on/copy/off`; SPS/AudioSpecificConfig parsing still missing (needed by HLS/stat). Extra: Enhanced RTMP (HEVC/AV1) |
| `ngx_rtmp_access_module.c` | `src/config.rs`, `src/session.rs` | ✅ allow/deny with CIDR |
| `ngx_rtmp_shared.c`, `ngx_rtmp_bandwidth.c` | — | not needed (`Bytes` is refcounted) / basic counters in `live::StreamStats` |
| `ngx_rtmp_proxy_protocol.c` | — | ⬜ |
| `ngx_rtmp_limit_module.c` | — | ⬜ `max_connections` |
| `ngx_rtmp_record_module.c` + `ngx_rtmp_flv_module.c` | — | ⬜ FLV recording |
| `ngx_rtmp_play_module.c`, `ngx_rtmp_mp4_module.c` | — | ⬜ VOD (`play /path`) |
| `ngx_rtmp_relay_module.c` | `src/relay.rs` | ✅ `push`, `pull`, `pull ... static`, `push_reconnect`, `pull_reconnect`, `relay_buffer`; all URL parameters (`app`, `name`, `playPath`, `tcUrl`, `pageUrl`, `swfUrl`, `flashVer`, `live`, `start`, `stop`, `static`) |
| `ngx_rtmp_notify_module.c`, `ngx_rtmp_netcall_module.c` | `src/notify.rs`, `src/session.rs` | ✅ `on_connect`, `on_disconnect`, `on_publish`, `on_play`, `on_publish_done`, `on_play_done`, `on_done`, `on_update`, `notify_method`, `notify_update_timeout`, `notify_update_strict`, `notify_relay_redirect`, `netcall_timeout`; 3xx redirects to a new app/stream name or to an `rtmp://` URL (push/pull). `on_record_done` is parsed but waits for `record` |
| `ngx_rtmp_exec_module.c` | — | ⬜ `exec*` |
| `ngx_rtmp_auto_push_module.c` | — | not applicable (nginx multi-worker); tokio runs a single process |
| `ngx_rtmp_log_module.c` | `access_log.rs` | ✅ `access_log`, `log_format`, all variables |
| `ngx_rtmp_stat_module.c`, `ngx_rtmp_control_module.c` | — | ⬜ need an HTTP endpoint of their own |
| `hls/` | — | ⬜ MPEG-TS + m3u8 |
| `dash/` | — | ⬜ fMP4 + MPD |

## Known differences

- Timestamps are forwarded as absolute values; `play_time_fix` does not rewrite the
  player timeline yet.
- If several `server {}` blocks share an address, the first one wins (RTMP has no
  virtual hosts).
- Relay: `session_relay` is accepted but has no effect (relay connections are always
  per stream). `push`/`pull` directives written in `server {}`/`rtmp {}` are inherited by
  applications (in nginx they stay in the configuration level where they are written).
- Non-static pull: if the upstream drops, local players are disconnected (same as
  nginx's "play disconnect orphan"); there is no automatic retry.
- `listen` accepts `port`, `ip:port`, `*:port`; extra options are ignored.
- Notify: while `on_connect` / `on_publish` / `on_play` is pending, the session stops
  reading from the client (nginx keeps reading and processes later commands). Relay
  connections never trigger callbacks. `on_disconnect` is only sent for sessions that
  issued `connect`. `on_play` always reports `start=0&duration=0&reset=0`.
  `notify_method` written inside `application {}` does not affect `on_connect` /
  `on_disconnect`. `netcall_buffer` is accepted and ignored.
- Access log: missing log directories are created (nginx fails to start). The implicit
  `logs/access.log` (no `access_log` anywhere) only applies to the binary, not to
  configurations built with `Config::parse` (tests, embedding). `log_format` is
  accepted at any level without nginx's warning. `$bytes_received` / `$bytes_sent` are
  64-bit totals after the handshake (nginx's 32-bit counter resets near 4 GB).
- Error log: only the first top-level `error_log` is used; `notice` maps to `info` and
  `crit`/`alert`/`emerg` to `error`. Unsupported directives are warnings, not
  `emerg`. `-s quit` exits at once like `-s stop` (no graceful drain);
  `-s reload` / `-s reopen` are not supported.

## Running

Like nginx, the binary has a prefix directory: relative paths in the configuration
(`access_log logs/access.log`) are resolved against it. It defaults to the directory
of the executable, and the configuration to `PREFIX/conf/nginx.conf`, so dropping
`rtmp-server.exe` into an nginx folder and running it with no arguments works.
`-p DIR` sets the prefix, `-c FILE` the configuration. Without `-p`, when
`PREFIX/conf/nginx.conf` doesn't exist (e.g. `cargo run`), `conf/nginx.conf` in the
working directory is used and the working directory becomes the prefix.

Started from Explorer (double-click, or `Start-Process` with a new window), the
process releases its console, so no window stays open; started from a terminal it
keeps logging there (filtered by `RUST_LOG`). Errors always go to the error log:
top-level `error_log file [level]`, default `PREFIX/logs/error.log` at level
`error`. The PID is written to `pid` (default `PREFIX/logs/nginx.pid`) and removed
on exit; `-s stop` / `-s quit` stops the running server (a named event on Windows,
SIGTERM / SIGQUIT elsewhere), so an existing `stop.bat` keeps working.

### Tray icon (Windows, not in nginx)

While serving, the binary shows an icon in the notification area (`src/tray.rs`, fed
by `src/monitor.rs` and `live::Registry::streams`):

- The nginx icon, with a red dot while any stream has a publisher.
- Tooltip: live streams, viewer count, total ↓/↑ bitrate.
- A toast when a stream goes live (resolution, frame rate, codecs, source address)
  and when it ends (duration, data received); can be turned off from the menu.
- Menu (left or right click): every stream with resolution, bitrate and viewers; per
  stream, copy its `rtmp://` URL (local and LAN), watch it in ffplay / VLC when
  installed, see and disconnect viewers or the publisher; every connection with its
  role, client, duration, bitrate and totals; open `nginx.conf` / the logs folder;
  stop the server.

## Testing

```bash
cargo test
cargo run --release -- -c conf/nginx.conf
ffmpeg -re -f lavfi -i testsrc -f lavfi -i sine -c:v libx264 -c:a aac -f flv rtmp://127.0.0.1/live/test
ffplay rtmp://127.0.0.1/live/test
```

## Suggested next steps

1. SPS/ASC parsing in `codec` → `hls`.
2. `record` (FLV) — reuses the `live` flow as is; then wire `on_record_done`.
3. `stat` / `control` (HTTP endpoint).
