//! Listener setup (`ngx_rtmp.c` / `ngx_rtmp_init.c` connection acceptance).

use crate::config::Config;
use crate::live::Registry;
use crate::session::{self, ServerCtx};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};

/// A bound server, ready to accept connections.
pub struct Server {
    listeners: Vec<(TcpListener, Arc<ServerCtx>)>,
    pub registry: Arc<Registry>,
}

impl Server {
    pub async fn bind(config: Config) -> std::io::Result<Self> {
        let registry = Arc::new(Registry::default());
        let mut listeners = Vec::new();
        let mut seen = HashSet::new();
        for (index, srv) in config.servers.into_iter().enumerate() {
            let addrs = srv.listen.clone();
            let ctx = Arc::new(ServerCtx::new(index, srv, registry.clone()));
            for addr in addrs {
                if !seen.insert(addr) {
                    // nginx would pick a server by name; RTMP has no virtual
                    // hosts, so the first server on an address wins.
                    info!("{addr} already bound by an earlier server block, skipping");
                    continue;
                }
                let l = TcpListener::bind(addr).await?;
                info!("listening on {}", l.local_addr()?);
                listeners.push((l, ctx.clone()));
            }
        }
        Ok(Self { listeners, registry })
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listeners.iter().filter_map(|(l, _)| l.local_addr().ok()).collect()
    }

    pub async fn run(self) {
        let mut tasks = Vec::new();
        let mut started = HashSet::new();
        for (l, ctx) in self.listeners {
            if started.insert(ctx.index) {
                crate::relay::start_static_pulls(&ctx);
            }
            tasks.push(tokio::spawn(accept_loop(l, ctx)));
        }
        for t in tasks {
            let _ = t.await;
        }
    }
}

async fn accept_loop(l: TcpListener, ctx: Arc<ServerCtx>) {
    loop {
        match l.accept().await {
            Ok((sock, addr)) => {
                let _ = sock.set_nodelay(true);
                if ctx.conf.so_keepalive {
                    let _ = socket2_keepalive(&sock);
                }
                tokio::spawn(session::serve(sock, addr, ctx.clone()));
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
