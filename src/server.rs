//! Listener setup (`ngx_rtmp.c` / `ngx_rtmp_init.c` connection acceptance)
//! and configuration reload.
//!
//! A reload works like `nginx -s reload` without worker processes: the new
//! configuration is read and every listener it needs is bound before
//! anything changes, so a configuration that fails (a bad certificate, a
//! port in use) leaves the running one untouched. Then new connections get
//! the new settings while existing ones keep the settings they started
//! with. Listeners on unchanged addresses are kept (no connection is
//! refused while reloading), new ones are opened and the ones that are no
//! longer configured are closed. Static pulls and `exec_static` commands
//! keep running when their configuration is the same and are restarted
//! when it changed.

use crate::config::{AppConf, Config, RelayTarget};
use crate::exec::{ExecConf, MainExec, Managed};
use crate::http::HttpCtx;
use crate::live::Registry;
use crate::session::{self, ServerCtx};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};

/// What an RTMP listener hands new connections to.
type RtmpTarget = (Arc<ServerCtx>, Option<TlsAcceptor>);

/// A listener is identified by the address it was configured with; port 0
/// (any free port) entries are told apart by their position.
type Key = (SocketAddr, usize);

/// Aborts its task when dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Slot<T> {
    local: SocketAddr,
    tx: watch::Sender<T>,
    /// Bound but not accepting yet (before [`Server::run`]).
    listener: Option<TcpListener>,
    task: Option<Task>,
}

struct PullSlot {
    ctx: Arc<ServerCtx>,
    app: Arc<AppConf>,
    target: Arc<RelayTarget>,
    task: Option<Task>,
}

struct StaticSlot {
    exec: Arc<MainExec>,
    conf: Arc<ExecConf>,
    running: Option<Managed>,
}

/// Bound addresses with the index of their `server {}` block and whether
/// they take RTMPS.
pub type ListenInfo = Vec<(usize, SocketAddr, bool)>;

/// A configuration to apply and where to report the outcome.
type ReloadRequest = (Config, oneshot::Sender<Result<(), String>>);

/// Asks the running server to apply a new configuration.
#[derive(Clone)]
pub struct Reloader(mpsc::UnboundedSender<ReloadRequest>);

impl Reloader {
    /// Apply `config`; on error the running configuration is kept.
    pub async fn reload(&self, config: Config) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.0.send((config, tx)).map_err(|_| "the server is not running".to_string())?;
        rx.await.map_err(|_| "the server stopped".to_string())?
    }
}

/// A bound server, ready to accept connections.
pub struct Server {
    pub registry: Arc<Registry>,
    rtmp: HashMap<Key, Slot<RtmpTarget>>,
    http: HashMap<Key, Slot<Arc<HttpCtx>>>,
    pulls: HashMap<String, PullSlot>,
    statics: HashMap<String, StaticSlot>,
    /// Current `server {}` contexts, by index (reused pieces on reload).
    ctxs: Vec<Arc<ServerCtx>>,
    running: bool,
    listen: Arc<RwLock<ListenInfo>>,
    reload_tx: mpsc::UnboundedSender<ReloadRequest>,
    reload_rx: Option<mpsc::UnboundedReceiver<ReloadRequest>>,
}

/// A configuration ready to be applied: contexts built and every new
/// listener bound.
struct Prepared {
    ctxs: Vec<Arc<ServerCtx>>,
    rtmp: Vec<(Key, RtmpTarget, Option<TcpListener>)>,
    http: Vec<(Key, Arc<HttpCtx>, Option<TcpListener>)>,
    pulls: Vec<(String, PullSlot)>,
    statics: Vec<(String, StaticSlot)>,
}

impl Server {
    pub async fn bind(config: Config) -> std::io::Result<Self> {
        let (reload_tx, reload_rx) = mpsc::unbounded_channel();
        let mut s = Self {
            registry: Arc::new(Registry::default()),
            rtmp: HashMap::new(),
            http: HashMap::new(),
            pulls: HashMap::new(),
            statics: HashMap::new(),
            ctxs: Vec::new(),
            running: false,
            listen: Arc::default(),
            reload_tx,
            reload_rx: Some(reload_rx),
        };
        let p = s.prepare(config).await?;
        s.apply(p);
        Ok(s)
    }

    /// A handle to reload the configuration once [`Server::run`] is running.
    pub fn reloader(&self) -> Reloader {
        Reloader(self.reload_tx.clone())
    }

    /// Bound addresses of the `http {}` servers.
    pub fn http_addrs(&self) -> Vec<SocketAddr> {
        let mut v: Vec<_> = self.http.values().map(|s| s.local).collect();
        v.sort();
        v
    }

    /// Bound RTMP addresses, kept up to date across reloads.
    pub fn listen_info_shared(&self) -> Arc<RwLock<ListenInfo>> {
        self.listen.clone()
    }

    pub fn listen_info(&self) -> ListenInfo {
        self.listen.read().unwrap().clone()
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listen_info().into_iter().map(|(_, a, _)| a).collect()
    }

    async fn prepare(&self, mut config: Config) -> std::io::Result<Prepared> {
        crate::access_log::open_all(&mut config)?;
        let prefix = config.prefix.clone();
        let exec = Arc::new(MainExec { prefix: prefix.clone(), ..config.exec.clone() });
        let mut ctxs = Vec::new();
        let mut rtmp = Vec::new();
        let mut seen = HashSet::new();
        let mut zero = 0;
        for (index, srv) in config.servers.into_iter().enumerate() {
            let addrs = srv.listen.clone();
            let acceptor = match (&srv.ssl_certificate, &srv.ssl_certificate_key) {
                (Some(cert), Some(key)) if srv.listen_ssl.contains(&true) => Some(crate::tls::acceptor(
                    &crate::access_log::resolve_path(prefix.as_deref(), cert),
                    &crate::access_log::resolve_path(prefix.as_deref(), key),
                )?),
                _ => None,
            };
            let ssl = srv.listen_ssl.clone();
            let mut ctx = ServerCtx::new(index, srv, self.registry.clone());
            ctx.exec = exec.clone();
            // Running exec_pull children stay known to the new settings.
            if let Some(old) = self.ctxs.get(index) {
                ctx.exec_pulls = old.exec_pulls.clone();
            }
            let ctx = Arc::new(ctx);
            ctxs.push(ctx.clone());
            for (addr, ssl) in addrs.into_iter().zip(ssl) {
                let key = if addr.port() == 0 {
                    zero += 1;
                    (addr, zero)
                } else if !seen.insert(addr) {
                    // nginx would pick a server by name; RTMP has no virtual
                    // hosts, so the first server on an address wins.
                    info!("{addr} already bound by an earlier server block, skipping");
                    continue;
                } else {
                    (addr, 0)
                };
                let tls = if ssl { acceptor.clone() } else { None };
                let listener = match self.rtmp.get(&key) {
                    Some(_) => None,
                    None => Some(
                        TcpListener::bind(addr)
                            .await
                            .map_err(|e| std::io::Error::new(e.kind(), format!("bind {addr} failed: {e}")))?,
                    ),
                };
                rtmp.push((key, (ctx.clone(), tls), listener));
            }
        }

        let servers = Arc::new(ctxs.clone());
        let mut http = Vec::new();
        let mut seen = HashSet::new();
        let mut zero = 0;
        for conf in config.http {
            if !conf.has_rtmp() {
                debug!("http server on {:?} has no rtmp_stat / rtmp_control, not started", conf.listen);
                continue;
            }
            let addrs = conf.listen.clone();
            let ctx = Arc::new(HttpCtx {
                conf,
                servers: servers.clone(),
                registry: self.registry.clone(),
                prefix: prefix.clone(),
            });
            for addr in addrs {
                let key = if addr.port() == 0 {
                    zero += 1;
                    (addr, zero)
                } else if !seen.insert(addr) {
                    info!("http: {addr} already bound by an earlier server block, skipping");
                    continue;
                } else {
                    (addr, 0)
                };
                let listener = match self.http.get(&key) {
                    Some(_) => None,
                    None => Some(
                        TcpListener::bind(addr)
                            .await
                            .map_err(|e| std::io::Error::new(e.kind(), format!("bind {addr} (http) failed: {e}")))?,
                    ),
                };
                http.push((key, ctx.clone(), listener));
            }
        }

        let mut pulls = Vec::new();
        for ctx in &ctxs {
            for app in &ctx.apps {
                for t in &app.static_pulls {
                    let key = format!("{}/{}/{:?}", ctx.index, app.name, t);
                    pulls.push((key, PullSlot { ctx: ctx.clone(), app: app.clone(), target: t.clone(), task: None }));
                }
            }
        }
        let mut statics = Vec::new();
        let mut count: HashMap<String, usize> = HashMap::new();
        for c in &exec.statics {
            let base = format!("{:?}|{:?}|{:?}|{:?}", c, exec.respawn_timeout, exec.kill_signal, exec.prefix);
            let n = count.entry(base.clone()).or_default();
            *n += 1;
            statics.push((format!("{base}#{n}"), StaticSlot { exec: exec.clone(), conf: c.clone(), running: None }));
        }
        Ok(Prepared { ctxs, rtmp, http, pulls, statics })
    }

    fn apply(&mut self, p: Prepared) {
        let mut rtmp = HashMap::new();
        for (key, target, listener) in p.rtmp {
            let slot = match (self.rtmp.remove(&key), listener) {
                (Some(old), _) => {
                    let _ = old.tx.send(target);
                    old
                }
                (None, Some(l)) => {
                    let local = l.local_addr().unwrap_or(key.0);
                    let tls = target.1.is_some();
                    info!("listening on {local}{}", if tls { " (rtmps)" } else { "" });
                    let (tx, _) = watch::channel(target);
                    let mut s = Slot { local, tx, listener: Some(l), task: None };
                    if self.running {
                        start_rtmp(&mut s);
                    }
                    s
                }
                (None, None) => continue,
            };
            rtmp.insert(key, slot);
        }
        for (_, old) in self.rtmp.drain() {
            info!("closing {}", old.local);
        }
        self.rtmp = rtmp;

        let mut http = HashMap::new();
        for (key, ctx, listener) in p.http {
            let slot = match (self.http.remove(&key), listener) {
                (Some(old), _) => {
                    let _ = old.tx.send(ctx);
                    old
                }
                (None, Some(l)) => {
                    let local = l.local_addr().unwrap_or(key.0);
                    info!("listening on {local} (http)");
                    let (tx, _) = watch::channel(ctx);
                    let mut s = Slot { local, tx, listener: Some(l), task: None };
                    if self.running {
                        start_http(&mut s);
                    }
                    s
                }
                (None, None) => continue,
            };
            http.insert(key, slot);
        }
        for (_, old) in self.http.drain() {
            info!("closing {} (http)", old.local);
        }
        self.http = http;

        let mut pulls = HashMap::new();
        for (key, mut slot) in p.pulls {
            if let Some(old) = self.pulls.remove(&key) {
                slot = old;
            } else if self.running {
                start_pull(&mut slot);
            }
            pulls.insert(key, slot);
        }
        for (key, _) in self.pulls.drain() {
            debug!("static pull removed: {key}");
        }
        self.pulls = pulls;

        let mut statics = HashMap::new();
        for (key, mut slot) in p.statics {
            if let Some(old) = self.statics.remove(&key) {
                slot = old;
            } else if self.running {
                slot.running = Some(slot.exec.start_static(&slot.conf));
            }
            statics.insert(key, slot);
        }
        self.statics.clear(); // stops the removed commands
        self.statics = statics;

        self.ctxs = p.ctxs;
        let mut info: ListenInfo =
            self.rtmp.values().map(|s| (s.tx.borrow().0.index, s.local, s.tx.borrow().1.is_some())).collect();
        info.sort_by_key(|(i, a, _)| (*i, *a));
        *self.listen.write().unwrap() = info;
    }

    /// Accept connections and run static pulls and `exec_static` commands,
    /// applying reloads sent through [`Server::reloader`].
    pub async fn run(mut self) {
        self.running = true;
        for s in self.rtmp.values_mut() {
            start_rtmp(s);
        }
        for s in self.http.values_mut() {
            start_http(s);
        }
        for s in self.pulls.values_mut() {
            start_pull(s);
        }
        for s in self.statics.values_mut() {
            s.running = Some(s.exec.start_static(&s.conf));
        }
        let mut rx = self.reload_rx.take().expect("run called once");
        while let Some((config, reply)) = rx.recv().await {
            let r = match self.prepare(config).await {
                Ok(p) => {
                    self.apply(p);
                    info!("configuration reloaded");
                    Ok(())
                }
                Err(e) => {
                    error!("reload failed, keeping the running configuration: {e}");
                    Err(e.to_string())
                }
            };
            let _ = reply.send(r);
        }
        // Every Reloader is gone (only when the Server's own sender is
        // dropped, i.e. never while running): keep serving.
        std::future::pending::<()>().await;
    }
}

fn start_rtmp(s: &mut Slot<RtmpTarget>) {
    if let Some(l) = s.listener.take() {
        s.task = Some(Task(tokio::spawn(accept_loop(l, s.tx.subscribe()))));
    }
}

fn start_http(s: &mut Slot<Arc<HttpCtx>>) {
    if let Some(l) = s.listener.take() {
        s.task = Some(Task(tokio::spawn(crate::http::accept_loop(l, s.tx.subscribe()))));
    }
}

fn start_pull(s: &mut PullSlot) {
    s.task = Some(Task(crate::relay::spawn_static_pull(s.ctx.clone(), s.app.clone(), s.target.clone())));
}

async fn accept_loop(l: TcpListener, target: watch::Receiver<RtmpTarget>) {
    loop {
        match l.accept().await {
            Ok((sock, addr)) => {
                let (ctx, tls) = target.borrow().clone();
                let _ = sock.set_nodelay(true);
                if ctx.conf.so_keepalive {
                    let _ = socket2_keepalive(&sock);
                }
                match tls {
                    None => {
                        tokio::spawn(session::serve(sock, addr, ctx));
                    }
                    Some(acceptor) => {
                        tokio::spawn(async move {
                            match tokio::time::timeout(ctx.conf.timeout, acceptor.accept(sock)).await {
                                Ok(Ok(stream)) => session::serve(stream, addr, ctx).await,
                                Ok(Err(e)) => info!("{addr}: TLS handshake failed: {e}"),
                                Err(_) => info!("{addr}: TLS handshake timed out"),
                            }
                        });
                    }
                }
            }
            Err(e) => {
                error!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

fn socket2_keepalive(_sock: &tokio::net::TcpStream) -> std::io::Result<()> {
    // TODO: SO_KEEPALIVE needs the socket2 crate; accepted but not applied yet.
    Ok(())
}
