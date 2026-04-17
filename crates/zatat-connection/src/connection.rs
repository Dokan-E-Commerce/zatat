use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::{mpsc, Notify};

use zatat_core::application::AppArc;
use zatat_core::id::SocketId;

#[derive(Clone, Debug)]
pub enum Outbound {
    /// Pre-serialized JSON frame, shared across recipients of a broadcast.
    Text(Arc<str>),
    Ping,
    Close {
        code: u16,
        reason: String,
    },
}

#[derive(Clone)]
pub struct ConnectionHandle {
    socket_id: SocketId,
    tx: mpsc::Sender<Outbound>,
    /// Fired when the connection must be torn down — e.g. the outbound
    /// mpsc filled up (client too slow) and we decided to kick the socket.
    kick: Arc<Notify>,
}

impl ConnectionHandle {
    pub fn from_parts(socket_id: SocketId, tx: mpsc::Sender<Outbound>, kick: Arc<Notify>) -> Self {
        Self {
            socket_id,
            tx,
            kick,
        }
    }

    pub fn socket_id(&self) -> &SocketId {
        &self.socket_id
    }

    /// Non-blocking send. Returns `false` on closed or full.
    /// On full, also fires the `kick` signal so the connection task closes
    /// the socket — slow clients must not silently miss broadcasts.
    pub fn try_send(&self, msg: Outbound) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // notify_one persists until consumed, so the connection task
                // sees the signal even if it was awaiting some other branch.
                self.kick.notify_one();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

impl std::fmt::Debug for ConnectionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionHandle")
            .field("socket_id", &self.socket_id)
            .finish_non_exhaustive()
    }
}

pub struct Connection {
    pub app: AppArc,
    pub socket_id: SocketId,
    pub origin: Option<String>,
    last_seen_at: AtomicI64,
    has_been_pinged: AtomicBool,
    user_id: Mutex<Option<String>>,
    watchlist: Mutex<Vec<String>>,
}

impl Connection {
    pub fn new(app: AppArc, socket_id: SocketId, origin: Option<String>) -> Self {
        Self {
            app,
            socket_id,
            origin,
            last_seen_at: AtomicI64::new(now_seconds()),
            has_been_pinged: AtomicBool::new(false),
            user_id: Mutex::new(None),
            watchlist: Mutex::new(Vec::new()),
        }
    }

    pub fn touch(&self) {
        self.last_seen_at.store(now_seconds(), Ordering::Relaxed);
        self.has_been_pinged.store(false, Ordering::Relaxed);
    }

    pub fn mark_pinged(&self) {
        self.has_been_pinged.store(true, Ordering::Relaxed);
    }

    pub fn has_been_pinged(&self) -> bool {
        self.has_been_pinged.load(Ordering::Relaxed)
    }

    pub fn last_seen_at(&self) -> i64 {
        self.last_seen_at.load(Ordering::Relaxed)
    }

    pub fn is_inactive(&self) -> bool {
        let idle = now_seconds() - self.last_seen_at();
        idle > self.app.activity_timeout as i64
    }

    pub fn is_stale(&self) -> bool {
        self.has_been_pinged() && self.is_inactive()
    }

    pub fn bind_user(&self, user_id: String) {
        *self.user_id.lock() = Some(user_id);
    }

    pub fn unbind_user(&self) {
        *self.user_id.lock() = None;
    }

    pub fn user_id(&self) -> Option<String> {
        self.user_id.lock().clone()
    }

    pub fn set_watchlist(&self, list: Vec<String>) {
        *self.watchlist.lock() = list;
    }

    pub fn watchlist(&self) -> Vec<String> {
        self.watchlist.lock().clone()
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
