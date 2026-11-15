//! Listener setup (`ngx_rtmp.c` / `ngx_rtmp_init.c` connection acceptance).

use crate::config::Config;
use crate::live::Registry;
use crate::session::{self, ServerCtx};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info};

/// A bound server, ready to accept connections.
pub struct Server {
    listeners: Vec<(TcpListener, Arc<ServerCtx>, Option<TlsAcceptor>)>,
    pub registry: Arc<Registry>,
    exec: Arc<crate::exec::MainExec>,
}

impl Server {
    pub async fn bind(mut config: Config) -> std::io::Result<Self> {
        crate::access_log::open_all(&mut config)?;
        let registry = Arc::new(Registry::default());
        let mut listeners = Vec::new();
        let mut seen = HashSet::new();
        let prefix = config.prefix.clone();
        let exec = Arc::new(crate::exec::MainExec { prefix: prefix.clone(), ..config.exec.clone() });
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
            let mut ctx = ServerCtx::new(index, srv, registry.clone());
            ctx.exec = exec.clone();
            let ctx = Arc::new(ctx);
            for (addr, ssl) in addrs.into_iter().zip(ssl) {
                // Port 0 (any free port) never clashes.
                if addr.port() != 0 && !seen.insert(addr) {
                    // nginx would pick a server by name; RTMP has no virtual
                    // hosts, so the first server on an address wins.
                    info!("{addr} already bound by an earlier server block, skipping");
                    continue;
                }
                let l = TcpListener::bind(addr)
                    .await
                    .map_err(|e| std::io::Error::new(e.kind(), format!("bind {addr} failed: {e}")))?;
                let tls = if ssl { acceptor.clone() } else { None };
                info!("listening on {}{}", l.local_addr()?, if tls.is_some() { " (rtmps)" } else { "" });
                listeners.push((l, ctx.clone(), tls));
            }
        }
        Ok(Self { listeners, registry, exec })
    }

    /// Bound addresses with the index of their `server {}` block and
    /// whether they take RTMPS.
    pub fn listen_info(&self) -> Vec<(usize, SocketAddr, bool)> {
        self.listeners
            .iter()
            .filter_map(|(l, c, t)| Some((c.index, l.local_addr().ok()?, t.is_some())))
            .collect()
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listeners.iter().filter_map(|(l, _, _)| l.local_addr().ok()).collect()
    }

    pub async fn run(self) {
        // exec_static children live as long as this future.
        let _statics = self.exec.start_statics();
        let mut tasks = Vec::new();
        let mut started = HashSet::new();
        for (l, ctx, tls) in self.listeners {
            if started.insert(ctx.index) {
                crate::relay::start_static_pulls(&ctx);
            }
            tasks.push(tokio::spawn(accept_loop(l, ctx, tls)));
        }
        for t in tasks {
            let _ = t.await;
        }
    }
}

async fn accept_loop(l: TcpListener, ctx: Arc<ServerCtx>, tls: Option<TlsAcceptor>) {
    loop {
        match l.accept().await {
            Ok((sock, addr)) => {
                let _ = sock.set_nodelay(true);
                if ctx.conf.so_keepalive {
                    let _ = socket2_keepalive(&sock);
                }
                match &tls {
                    None => {
                        tokio::spawn(session::serve(sock, addr, ctx.clone()));
                    }
                    Some(acceptor) => {
                        let (acceptor, ctx) = (acceptor.clone(), ctx.clone());
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
