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
| `ngx_rtmp_codec_module.c` | `src/codec.rs` | ✅ AAC/AVC sequence headers, SPS and AudioSpecificConfig parsing (profile, level, size, sample rate, channels, SBR/PS), `meta on/copy/off`. Extra: Enhanced RTMP (HEVC/AV1) |
| `ngx_rtmp_access_module.c` | `src/config.rs`, `src/session.rs` | ✅ allow/deny with CIDR |
| `ngx_rtmp_shared.c`, `ngx_rtmp_bandwidth.c` | — | not needed (`Bytes` is refcounted) / basic counters in `live::StreamStats` |
| `ngx_rtmp_proxy_protocol.c` | — | ⬜ |
| `ngx_rtmp_limit_module.c` | — | ⬜ `max_connections` |
| `ngx_rtmp_record_module.c` | `src/record.rs` | ✅ `record` (`all`, `audio`, `video`, `keyframes`, `manual`, `off`), `record_path`, `record_suffix` (with `strftime` patterns), `record_unique`, `record_append`, `record_lock`, `record_max_size`, `record_max_frames`, `record_interval`, `record_notify`, `recorder` blocks; pulled streams are recorded too. `manual` recorders are started and stopped through the control module |
| `ngx_rtmp_play_module.c`, `ngx_rtmp_flv_module.c`, `ngx_rtmp_mp4_module.c` | — | ⬜ VOD (`play /path`) |
| `ngx_rtmp_relay_module.c` | `src/relay.rs` | ✅ `push`, `pull` (also `rtmps://`, see below), `pull ... static`, `push_reconnect`, `pull_reconnect`, `relay_buffer`; all URL parameters (`app`, `name`, `playPath`, `tcUrl`, `pageUrl`, `swfUrl`, `flashVer`, `live`, `start`, `stop`, `static`) |
| `ngx_rtmp_notify_module.c`, `ngx_rtmp_netcall_module.c` | `src/notify.rs`, `src/session.rs` | ✅ `on_connect`, `on_disconnect`, `on_publish`, `on_play`, `on_publish_done`, `on_play_done`, `on_done`, `on_update`, `notify_method`, `notify_update_timeout`, `notify_update_strict`, `notify_relay_redirect`, `netcall_timeout`; 3xx redirects to a new app/stream name or to an `rtmp://` URL (push/pull). `on_record_done` |
| `ngx_rtmp_exec_module.c` | `src/exec.rs` | ✅ `exec`, `exec_push`, `exec_pull`, `exec_static`, `exec_publish`, `exec_publish_done`, `exec_play`, `exec_play_done`, `respawn`, `respawn_timeout`, `exec_kill_signal`, `exec_options` (`name=`); `$var` arguments and `>file` / `2>>file` / `2>&1` redirection. Works on Windows (the C module is compiled out there). `exec_record_done` (with `$recorder`, `$path`, `$filename`, `$basename`, `$dirname`) |
| `ngx_rtmp_auto_push_module.c` | — | not applicable (nginx multi-worker); tokio runs a single process |
| `ngx_rtmp_log_module.c` | `access_log.rs` | ✅ `access_log`, `log_format`, all variables |
| `ngx_rtmp_stat_module.c` | `src/stat.rs`, `src/http.rs` | ✅ `rtmp_stat` (`all`, `global`, `live`, `clients`), `rtmp_stat_stylesheet`; same XML as nginx, so `stat.xsl` (shipped in `html/`) works |
| `ngx_rtmp_control_module.c` | `src/control.rs`, `src/http.rs` | ✅ `rtmp_control` (`all`, `record`, `drop`, `redirect`): `record/start|stop`, `drop/publisher|subscriber|client`, `redirect/publisher|subscriber|client` with `srv`, `app`, `name`, `addr`, `clientid`, `rec`, `newname` |
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
- `listen` accepts `port`, `ip:port`, `*:port` and the `ssl` flag; other options are ignored.
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
  `-s reopen` is not supported.
- Reload (`-s reload`, the tray icon, the editor): there are no worker processes, so
  the new configuration applies to new connections and existing ones keep the
  settings they started with. Every new listener is bound before anything changes;
  if one fails (or the file has an error) the running configuration is kept.
  Listeners on unchanged addresses stay open. Static pulls and `exec_static`
  commands are restarted only when their own settings changed. `pid` changes take
  effect at the next start.

- Exec: `exec_push` children start after the publish succeeds (nginx starts them
  before the live module, so a rejected second publisher briefly runs its own copy).
  Only file descriptors 0-2 can be redirected. On Windows a backslash in arguments is
  a path separator, not an escape (`C:\rec\$name.flv`). `name=` filters match any of
  the listed names (nginx compares only the first).
- Record: files are opened after the publish succeeds (nginx opens them first, so a
  rejected second publisher truncates the file being recorded). Relative
  `record_path` values resolve against the prefix, an empty one means the prefix
  itself (nginx would write to `/`), and missing directories are created. An AAC
  sequence header is written once (nginx writes it twice at the start of a file).
  The reported path uses the platform's separator before the file name
  (`C:\rec\cam.flv`). `record_lock` does nothing on Windows, as in nginx.

- HTTP (stat, control): the `http {}` block is read for `server` / `listen`,
  `location` (prefix, `=` and `^~`; regular expressions and named locations are
  skipped with a warning), `root`, `allow` / `deny`, `rtmp_stat`,
  `rtmp_stat_stylesheet` and `rtmp_control`; every other directive is ignored. Only
  servers that use `rtmp_stat` or `rtmp_control` are started, so an `http {}` block
  serving something else does not take its ports. Other locations of those servers
  serve static files from `root` (default `html`, relative to the prefix). No
  `listen ... ssl`, no keep-alive (each response closes the connection). Query
  arguments are URL-decoded (nginx compares them raw).
- Stat: `<nginx_version>` reports `nginx-rtmp-module-rs/VERSION` and `<built>` the
  executable's modification time. Non-ASCII text is kept as UTF-8 (nginx turns
  every non-ASCII byte into a space). In each stream the publisher is listed first.
  Relay connections show their upstream URL as `<address>`; the global byte
  counters only count client connections. The SPS parser skips scaling lists
  (nginx misreads the size of streams that carry them).
- Control: a session handles `record` and `redirect` requests in its own task;
  the HTTP request waits up to 30 s for each. `record` on a pulled stream works;
  relays are never redirected and not counted by `redirect`.

## Running

Like nginx, the binary has a prefix directory: relative paths in the configuration
(`access_log logs/access.log`) are resolved against it. It defaults to the directory
of the executable, and the configuration to `PREFIX/conf/nginx.conf`, so dropping
the binary (`nginx.exe`) into an nginx folder and running it with no arguments works.
`-p DIR` sets the prefix, `-c FILE` the configuration. Without `-p`, when
`PREFIX/conf/nginx.conf` doesn't exist (e.g. `cargo run`), `conf/nginx.conf` in the
working directory is used and the working directory becomes the prefix.

Started from Explorer (double-click, or `Start-Process` with a new window), the
process releases its console, so no window stays open; started from a terminal it
keeps logging there (filtered by `RUST_LOG`). Errors always go to the error log:
top-level `error_log file [level]`, default `PREFIX/logs/error.log` at level
`error`. The PID is written to `pid` (default `PREFIX/logs/nginx.pid`) and removed
on exit; `-s stop` / `-s quit` stops the running server and `-s reload` makes it
read the configuration again (named events on Windows, SIGTERM / SIGQUIT / SIGHUP
elsewhere), so an existing `stop.bat` keeps working.

### Configuration editor (not in nginx)

`nginx -e`, or "Configure…" in the tray menu, opens `nginx.conf` in a window of the
application on Windows, drawn with [iced](https://iced.rs) (`src/editor_ui.rs`,
`src/editor_ui/`). iced must run on a process's main thread and only once per process,
so the window is a process of its own, `nginx.exe --editor-ui`: it reads the editor's
address from its standard input, loads, saves and reloads through the editor API
(`src/editor.rs`, served on 127.0.0.1 with a random token in the URL) and checks the
text in-process with the same logic (`src/conf_form.rs`). It ends when the window is
closed or when the program that started it exits; opening it again brings the open
window to the front. Closing it with unsaved changes asks first; with `-e` the program
ends when the window is closed. Elsewhere, or if the window cannot be opened, the same
editor is shown as a web page in the browser (`src/editor.html`). The window is light or
dark like Windows' app mode; `NGINX_EDITOR_THEME=light` or `dark` picks one.

- A tree of the blocks (`rtmp`, `server`, `application`, `recorder`, `http`,
  `location`) and, for the selected one, a form with every directive: on/off
  switches, choices, checkboxes for `record` / `rtmp_stat` / `listen ... ssl`,
  `key=value` rows for relay parameters, with the description and default of each
  directive (`src/conf_schema.rs`, which lists everything the server understands).
- Add, delete, reorder, disable (comment out) and enable directives; add blocks.
  Directives the server does not use are kept and can be edited as text.
- Every change is checked by the real parser; errors and warnings show at once.
- A Text tab edits the whole file directly.
- Save & reload checks the file, keeps the previous one as `nginx.conf.bak`, writes
  it and reloads the running server (with `-e`, by sending `-s reload`). A file
  changed by another program in the meantime is not overwritten without asking.
- Comments, blank lines and the spelling of untouched directives are kept
  (`src/conf_edit.rs`); only indentation is normalised to four spaces.

### RTMPS (not in nginx)

nginx-rtmp-module needs an external TLS proxy (stunnel, or nginx `stream { ssl }`)
for RTMPS. The port does it natively (`src/tls.rs`, rustls with `ring`):

- `push` / `pull` accept `rtmps://host[:port]/app/...` (default port 443), e.g.
  `push rtmps://live-api-s.facebook.com:443/rtmp/;` or
  `push rtmps://a.rtmps.youtube.com/live2/;`. Server certificates are checked
  against Mozilla's root list (`webpki-roots`) and the host name. The default
  `tcUrl` uses the `rtmps://` scheme. `on_publish` / `on_play` redirects may also
  return an `rtmps://` URL.
- `listen PORT ssl;` accepts RTMPS clients, with `ssl_certificate` and
  `ssl_certificate_key` (PEM, nginx syntax; relative paths resolve against the
  prefix) in `server {}` or `rtmp {}`. Plain and `ssl` listeners can be mixed.
- A TLS peer that closes without `close_notify` is treated as a normal close.

### exec on Windows (not in nginx)

nginx-rtmp-module compiles `exec*` out on Windows. Here it works the same as on Linux,
so ffmpeg relays or transcoders can be tied to a stream instead of being started by
hand:

```nginx
application live {
    live on;
    exec ffmpeg -i rtmp://127.0.0.1/live/$name -c:v libx264 -b:v 3000k -c:a copy
        -f flv rtmps://live-api-s.facebook.com:443/rtmp/KEY 2>>logs/ffmpeg-$name.log;
}
```

- Children never open a console window, even when the server runs without one.
- Each managed child (`exec`, `exec_pull`, `exec_static`) runs in its own Job Object:
  stopping it kills the whole tree (so `cmd /c script.bat` wrappers work), and if the
  server dies, even forcibly, its children die with it (Linux uses `prctl` for this).
- Children run in the prefix directory; a relative program path (`scripts/x.bat`) and
  redirect files are relative to the prefix, a bare name (`ffmpeg`) is looked up in
  `PATH`. Missing log directories for redirects are created.
- `exec_kill_signal` is accepted but Windows always terminates the process.

### Tray icon (Windows, not in nginx)

While serving, the binary shows an icon in the notification area (`src/tray.rs`, fed
by `src/monitor.rs` and `live::Registry::streams`):

- A green hexagon icon (assets/make_icons.py), with a red dot while any stream has a publisher.
- Tooltip: live streams, viewer count, total ↓/↑ bitrate.
- A toast when a stream goes live (resolution, frame rate, codecs, source address)
  and when it ends (duration, data received); can be turned off from the menu.
- Menu (left or right click): every stream with resolution, bitrate and viewers; per
  stream, copy its `rtmp://` URL (local and LAN), watch it in ffplay / VLC when
  installed, see and disconnect viewers or the publisher; every connection with its
  role, client, duration, bitrate and totals; open the configuration editor, reload
  the configuration, open `nginx.conf` / the logs folder; stop the server.

## Testing

```bash
cargo test
cargo run --release -- -c conf/nginx.conf
ffmpeg -re -f lavfi -i testsrc -f lavfi -i sine -c:v libx264 -c:a aac -f flv rtmp://127.0.0.1/live/test
ffplay rtmp://127.0.0.1/live/test
```

## Suggested next steps

1. `hls` (MPEG-TS + m3u8, served by the same HTTP server).
2. `max_connections` (limit module) and `proxy_protocol`.
3. VOD (`play` module: FLV / MP4 files).
