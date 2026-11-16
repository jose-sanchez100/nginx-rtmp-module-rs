//! Rust port of [nginx-rtmp-module](https://github.com/arut/nginx-rtmp-module).
//!
//! A standalone, async RTMP server that keeps nginx-rtmp's protocol behaviour
//! and reads the same `rtmp { ... }` configuration syntax.

pub mod access_log;
pub mod amf;
pub mod chunk;
pub mod codec;
pub mod conf_edit;
pub mod conf_form;
pub mod conf_help;
pub mod conf_schema;
pub mod config;
pub mod control;
pub mod editor;
pub mod exec;
pub mod handshake;
pub mod http;
pub mod live;
pub mod message;
pub mod monitor;
pub mod notify;
pub mod record;
pub mod relay;
pub mod server;
pub mod session;
pub mod stat;
pub mod tls;
