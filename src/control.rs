//! Remote control over HTTP.
//!
//! Port of `ngx_rtmp_control_module.c`: `rtmp_control` locations answer
//! `.../record/start|stop`, `.../drop/publisher|subscriber|client` and
//! `.../redirect/publisher|subscriber|client`, selecting sessions with the
//! `srv`, `app`, `name`, `addr` and `clientid` arguments.
//!
//! Dropping a session closes it from here. Recording and redirects run in
//! the session that owns the stream: it gets a [`Control`] message on its
//! control channel and answers through a oneshot channel.

use crate::live::Peer;
use crate::session::ServerCtx;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Request argument lookup (`ngx_http_arg`).
pub type Arg<'a> = dyn Fn(&str) -> Option<String> + Sync + 'a;

pub const RECORD: u8 = 0x01;
pub const DROP: u8 = 0x02;
pub const REDIRECT: u8 = 0x04;
pub const ALL: u8 = 0xff;

/// `rtmp_control` values.
pub fn parse_flag(v: &str) -> Option<u8> {
    Some(match v {
        "all" => ALL,
        "record" => RECORD,
        "drop" => DROP,
        "redirect" => REDIRECT,
        _ => return None,
    })
}

/// A request for the task that owns a session.
pub enum Control {
    /// `record/start` or `record/stop` for one recorder (`""` is the
    /// application's own). Answers the path of the file that was opened or
    /// closed, `None` when it was open (start) or closed (stop) already.
    Record { start: bool, recorder: String, reply: oneshot::Sender<Result<Option<String>, String>> },
    /// `redirect`: close the stream and publish or play `name` instead.
    Redirect { name: String, reply: oneshot::Sender<Result<(), String>> },
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Client,
    Publisher,
    Subscriber,
}

/// A control response: status and body.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    fn ok(body: String) -> Self {
        Self { status: 200, body }
    }

    fn error(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::info!("control: {msg}");
        Self { status: 500, body: msg }
    }
}

/// How long a session gets to carry out a record or redirect request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// `ngx_rtmp_control_handler`. `None` when the location does not handle
/// this request (unknown section or one not enabled by `rtmp_control`).
pub async fn handle(
    servers: &[Arc<ServerCtx>],
    mask: u8,
    path: &str,
    arg: &Arg<'_>,
) -> Option<Response> {
    // uri format: .../section/method
    let mut parts = path.rsplitn(3, '/');
    let method = parts.next().unwrap_or("");
    let section = parts.next().unwrap_or("");
    tracing::debug!("control: section='{section}' method='{method}'");
    let filter = || match method {
        "publisher" => Some(Filter::Publisher),
        "subscriber" => Some(Filter::Subscriber),
        "client" => Some(Filter::Client),
        _ => None,
    };
    Some(match section {
        "record" if mask & RECORD != 0 => record(servers, method, arg).await,
        "drop" if mask & DROP != 0 => match filter() {
            Some(f) => drop_sessions(servers, f, arg),
            None => Response::error("Undefined filter"),
        },
        "redirect" if mask & REDIRECT != 0 => match filter() {
            Some(f) => redirect(servers, f, arg).await,
            None => Response::error("Undefined filter"),
        },
        _ => return None,
    })
}

/// `ngx_rtmp_control_walk`: the sessions of the selected server that match
/// `app`, `name`, `addr`, `clientid` and the filter.
fn walk(servers: &[Arc<ServerCtx>], filter: Filter, arg: &Arg<'_>) -> Result<Vec<Peer>, String> {
    let sn = arg("srv").map_or(0, |s| s.parse::<usize>().unwrap_or(usize::MAX));
    let Some(ctx) = servers.get(sn) else {
        return Err("Server index out of range".into());
    };
    let app = arg("app").filter(|a| !a.is_empty());
    let name = arg("name");
    let addr = arg("addr");
    let clientid = arg("clientid").map(|c| c.parse::<u64>().unwrap_or(0));
    let mut out = Vec::new();
    for a in &ctx.apps {
        if app.as_ref().is_some_and(|n| *n != a.name) {
            continue;
        }
        for (peer, publishing) in ctx.registry.sessions(ctx.index, &a.name, name.as_deref()) {
            if addr.as_ref().is_some_and(|x| *x != peer.info.addr) || clientid.is_some_and(|c| c != peer.id) {
                continue;
            }
            match filter {
                Filter::Publisher if !publishing => continue,
                Filter::Subscriber if publishing => continue,
                _ => {}
            }
            out.push(peer);
        }
    }
    Ok(out)
}

async fn record(servers: &[Arc<ServerCtx>], method: &str, arg: &Arg<'_>) -> Response {
    let peers = match walk(servers, Filter::Publisher, arg) {
        Ok(p) => p,
        Err(e) => return Response::error(e),
    };
    // nginx checks the method for each session found, so with none an
    // unknown method is not an error either.
    let start = match method {
        "start" => true,
        "stop" => false,
        _ if peers.is_empty() => return Response { status: 204, body: String::new() },
        _ => return Response::error("Undefined method"),
    };
    let recorder = arg("rec").unwrap_or_default();
    let mut path = None;
    for p in peers {
        let Some(ctl) = &p.info.ctl else { continue };
        let (tx, rx) = oneshot::channel();
        if ctl.send(Control::Record { start, recorder: recorder.clone(), reply: tx }).is_err() {
            continue; // session gone
        }
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(Ok(Some(p)))) => path = Some(p),
            Ok(Ok(Ok(None))) | Ok(Err(_)) => {}
            Ok(Ok(Err(e))) => return Response::error(e),
            Err(_) => return Response::error("Recorder error"),
        }
    }
    match path {
        Some(p) => Response::ok(p),
        None => Response { status: 204, body: String::new() },
    }
}

fn drop_sessions(servers: &[Arc<ServerCtx>], filter: Filter, arg: &Arg<'_>) -> Response {
    match walk(servers, filter, arg) {
        Ok(peers) => {
            for p in &peers {
                tracing::info!("control: drop session {}", p.id);
                p.kill();
            }
            Response::ok(peers.len().to_string())
        }
        Err(e) => Response::error(e),
    }
}

async fn redirect(servers: &[Arc<ServerCtx>], filter: Filter, arg: &Arg<'_>) -> Response {
    let Some(name) = arg("newname") else {
        return Response::error("newname not specified");
    };
    let peers = match walk(servers, filter, arg) {
        Ok(p) => p,
        Err(e) => return Response::error(e),
    };
    let mut count = 0;
    for p in peers {
        // Relays (no `connect` from a client) are not redirected.
        let (Some(ctl), Some(_)) = (&p.info.ctl, p.info.connect.get()) else { continue };
        count += 1;
        let (tx, rx) = oneshot::channel();
        if ctl.send(Control::Redirect { name: name.clone(), reply: tx }).is_err() {
            continue;
        }
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(Err(e))) => return Response::error(e),
            Err(_) => return Response::error("redirect timed out"),
            _ => {}
        }
    }
    Response::ok(count.to_string())
}
