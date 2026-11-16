# nginx-rtmp-module-rs

A Rust port of [nginx-rtmp-module](https://github.com/arut/nginx-rtmp-module) as a
standalone RTMP server. It reads the `rtmp {}` (and, for stat/control, `http {}`)
blocks of an existing `nginx.conf` and builds a binary called `nginx`, so it can
replace nginx in a folder that only uses it for RTMP, on Windows as well as on
other platforms.

This is not an nginx module and does not need nginx: it runs on
[tokio](https://tokio.rs).

## Features

- RTMP handshake, chunking and commands; `live` applications with the same
  options as nginx-rtmp (`wait_key`, `wait_video`, `interleave`, `sync`,
  `publish_notify`, `idle_streams`, `drop_idle_publisher`, ...).
- Relays: `push`, `pull`, `pull ... static`, reconnects, all URL parameters.
- `notify` (HTTP callbacks), `exec` (also on Windows), `record` (FLV),
  `access_log` / `log_format`, `allow` / `deny`.
- Statistics (`rtmp_stat`, same XML as nginx, `html/stat.xsl` included) and
  remote control (`rtmp_control`: record, drop, redirect).
- RTMPS, which nginx-rtmp lacks: `rtmps://` relays (e.g. Facebook, YouTube) and
  `listen PORT ssl;`.
- Enhanced RTMP codecs (HEVC, AV1, VP9) are forwarded.
- Configuration editor in its own window (`nginx -e` or the tray menu) with a form for
  every supported directive, explanations of wrong values, settings that need each
  other, inherited values and one-click recipes (recording, relaying to YouTube or
  Twitch, a statistics page); saving reloads the running server without dropping
  connections (`-s reload` works too).
- On Windows: runs without a console like nginx, error log, pid file,
  `-s stop|quit|reload`, and a tray icon listing streams and connections.

Not ported yet: HLS, DASH, VOD (`play /path`), `max_connections`,
`proxy_protocol`. See [PORTING.md](PORTING.md) for the status of every C file
and the known differences from nginx.

## Build

```bash
cargo build --release
```

The binary is `target/release/nginx` (`nginx.exe` on Windows).

## Run

```bash
nginx [-p prefix] [-c nginx.conf] [-t] [-e] [-s stop|quit|reload]
```

`-e` opens the configuration editor in a window (on Windows; a page in the browser
elsewhere); `-s reload` applies a changed
`nginx.conf` to the running server, keeping current connections.

The prefix defaults to the executable's directory and the configuration to
`PREFIX/conf/nginx.conf`, so the binary can be dropped into an nginx folder and
started with no arguments. A minimal configuration:

```nginx
rtmp {
    server {
        listen 1935;
        application live {
            live on;
        }
    }
}
```

`conf/nginx.conf` has commented examples for relays, recording, stat and control.
Logging to the terminal follows `RUST_LOG` (e.g. `RUST_LOG=nginx_rtmp=debug`).

## Tests

```bash
cargo test
```

## License

BSD 2-Clause, the same as nginx-rtmp-module. See [LICENSE](LICENSE).
