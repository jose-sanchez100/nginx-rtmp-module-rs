//! Live view of the server for user interfaces (the tray icon): who is
//! connected, what each connection is doing, and byte counters. Not part of
//! nginx-rtmp-module; the closest equivalent there is `ngx_rtmp_stat_module`.

use crate::live::SessionId;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Handshake done, no stream yet.
    Idle,
    Publishing,
    Playing,
}

struct Conn {
    addr: SocketAddr,
    server: usize,
    started: Instant,
    app: String,
    name: String,
    role: Role,
    flashver: String,
    bytes_in: Arc<AtomicU64>,
    bytes_out: Arc<AtomicU64>,
    close: Arc<Notify>,
}

/// Point-in-time copy of one connection.
#[derive(Debug, Clone)]
pub struct ConnInfo {
    pub id: SessionId,
    pub addr: SocketAddr,
    pub server: usize,
    pub secs: u64,
    pub app: String,
    pub name: String,
    pub role: Role,
    pub flashver: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

#[derive(Default)]
pub struct Monitor {
    conns: Mutex<BTreeMap<SessionId, Conn>>,
}

impl Monitor {
    /// Track a connection until the returned guard is dropped.
    pub fn register(
        self: &Arc<Self>,
        id: SessionId,
        addr: SocketAddr,
        server: usize,
        bytes_in: Arc<AtomicU64>,
        bytes_out: Arc<AtomicU64>,
        close: Arc<Notify>,
    ) -> MonitorGuard {
        let conn = Conn {
            addr,
            server,
            started: Instant::now(),
            app: String::new(),
            name: String::new(),
            role: Role::Idle,
            flashver: String::new(),
            bytes_in,
            bytes_out,
            close,
        };
        self.conns.lock().unwrap().insert(id, conn);
        MonitorGuard { monitor: self.clone(), id }
    }

    pub fn set_app(&self, id: SessionId, app: &str, flashver: &str) {
        if let Some(c) = self.conns.lock().unwrap().get_mut(&id) {
            c.app = app.to_string();
            c.flashver = flashver.to_string();
        }
    }

    pub fn set_role(&self, id: SessionId, role: Role, name: &str) {
        if let Some(c) = self.conns.lock().unwrap().get_mut(&id) {
            c.role = role;
            c.name = name.to_string();
        }
    }

    /// Ask a connection to close. Returns false if it is gone already.
    pub fn disconnect(&self, id: SessionId) -> bool {
        match self.conns.lock().unwrap().get(&id) {
            Some(c) => {
                c.close.notify_one();
                true
            }
            None => false,
        }
    }

    pub fn snapshot(&self) -> Vec<ConnInfo> {
        let now = Instant::now();
        self.conns
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, c)| ConnInfo {
                id,
                addr: c.addr,
                server: c.server,
                secs: now.duration_since(c.started).as_secs(),
                app: c.app.clone(),
                name: c.name.clone(),
                role: c.role,
                flashver: c.flashver.clone(),
                bytes_in: c.bytes_in.load(Ordering::Relaxed),
                bytes_out: c.bytes_out.load(Ordering::Relaxed),
            })
            .collect()
    }
}

/// Removes the connection from the monitor when dropped.
pub struct MonitorGuard {
    monitor: Arc<Monitor>,
    id: SessionId,
}

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.monitor.conns.lock().unwrap().remove(&self.id);
    }
}
