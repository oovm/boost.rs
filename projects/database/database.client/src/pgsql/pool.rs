//! Connection lease state machine for the PostgreSQL wire client.
//!
//! The pool owns the only copy of leasing state: the `max` cap on live
//! leases, the `max_pending` admission queue, per-waiter acquire deadlines,
//! idle hand-back and the close drain. Hosts (N-API, CLI, tests) call
//! [`Pool::acquire`] / [`Pool::release`] / [`Pool::discard`] and must not
//! mirror any of these counters themselves.
//!
//! [`Pool::acquire`] blocks the calling thread while a lease is queued, so
//! hosts run it on a worker thread (an async task, a blocking task, …) and
//! never on a single-threaded event loop.

use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
    time::Duration,
};

use database_protocol_pgsql::PostgresError;

/// Admission and waiting limits of a [`Pool`].
#[derive(Debug, Clone)]
pub struct PoolLimits {
    /// Upper bound of simultaneously leased connections.
    pub max: usize,
    /// Upper bound of queued waiters; further acquires are rejected saturated.
    pub max_pending: usize,
    /// How long a queued waiter blocks before it is dropped. `None` waits
    /// until a lease is handed over or the pool closes.
    pub acquire_timeout: Option<Duration>,
}

impl Default for PoolLimits {
    fn default() -> Self {
        Self { max: 10, max_pending: 100, acquire_timeout: Some(Duration::from_secs(10)) }
    }
}

/// Why a lease was not granted.
#[derive(Debug, Clone)]
pub enum PoolError {
    /// The pool is closed: it was closed before, or while, the lease waited.
    Closed,
    /// `max_pending` waiters are already queued, so the acquire was refused.
    Saturated,
    /// The queued waiter outlived `acquire_timeout` without a lease.
    AcquireTimeout,
    /// Opening a new connection for the lease failed.
    Dial(PostgresError),
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the connection pool is closed"),
            Self::Saturated => f.write_str("the connection pool is saturated"),
            Self::AcquireTimeout => f.write_str("timed out waiting for a pooled connection"),
            Self::Dial(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PoolError {}

/// A snapshot of the leasing counters, for introspection and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// Connections currently handed out.
    pub in_use: usize,
    /// Connections returned and waiting for reuse.
    pub idle: usize,
    /// Waiters queued behind the `max` cap.
    pub waiting: usize,
}

type Handoff<C> = mpsc::Sender<Result<C, PoolError>>;

struct State<C> {
    idle: Vec<C>,
    in_use: usize,
    waiters: VecDeque<(u64, Handoff<C>)>,
    closed: bool,
    next_waiter: u64,
}

/// A pool of connections leased from a host-provided connector.
///
/// `C` is the connection handle the host wraps; the pool only ever moves it
/// between the dialer, the lease holders and the closer.
pub struct Pool<C> {
    limits: PoolLimits,
    connector: Arc<dyn Fn() -> Result<C, PostgresError> + Send + Sync>,
    closer: Arc<dyn Fn(C) + Send + Sync>,
    state: Mutex<State<C>>,
    drained: Condvar,
}

impl<C: Send> Pool<C> {
    /// Builds a pool. `connector` opens one connection and `closer` shuts
    /// one down for good; both run without the pool lock held.
    pub fn new(
        limits: PoolLimits,
        connector: impl Fn() -> Result<C, PostgresError> + Send + Sync + 'static,
        closer: impl Fn(C) + Send + Sync + 'static,
    ) -> Self {
        Self {
            limits,
            connector: Arc::new(connector),
            closer: Arc::new(closer),
            state: Mutex::new(State { idle: Vec::new(), in_use: 0, waiters: VecDeque::new(), closed: false, next_waiter: 0 }),
            drained: Condvar::new(),
        }
    }

    /// Takes one lease: an idle connection, a freshly dialled connection, or
    /// a place in the admission queue. Blocking; run on a worker thread.
    pub fn acquire(&self) -> Result<C, PoolError> {
        let mut state = self.lock();
        if state.closed {
            return Err(PoolError::Closed);
        }
        if let Some(connection) = state.idle.pop() {
            state.in_use += 1;
            return Ok(connection);
        }
        if state.in_use < self.limits.max {
            state.in_use += 1;
            drop(state);
            return match (self.connector)() {
                Ok(connection) => Ok(connection),
                Err(error) => {
                    let mut state = self.lock();
                    state.in_use -= 1;
                    if state.in_use == 0 {
                        self.drained.notify_all();
                    }
                    Err(PoolError::Dial(error))
                }
            };
        }
        if state.waiters.len() >= self.limits.max_pending {
            return Err(PoolError::Saturated);
        }
        let id = state.next_waiter;
        state.next_waiter += 1;
        let (handoff, lease) = mpsc::channel();
        state.waiters.push_back((id, handoff));
        drop(state);

        match self.limits.acquire_timeout {
            Some(timeout) => match lease.recv_timeout(timeout) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(PoolError::Closed),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let mut state = self.lock();
                    if state.waiters.iter().any(|(waiter, _)| *waiter == id) {
                        state.waiters.retain(|(waiter, _)| *waiter != id);
                        return Err(PoolError::AcquireTimeout);
                    }
                    // A release popped this entry to hand over a lease before
                    // the timeout was observed; the value is already queued.
                    drop(state);
                    lease.recv().unwrap_or(Err(PoolError::AcquireTimeout))
                }
            },
            None => lease.recv().unwrap_or(Err(PoolError::Closed)),
        }
    }

    /// Returns a healthy connection; the first queued waiter adopts it as its
    /// lease, otherwise it parks in the idle set.
    pub fn release(&self, connection: C) {
        let mut state = self.lock();
        let mut lease = Ok(connection);
        while let Some((_, handoff)) = state.waiters.pop_front() {
            lease = match handoff.send(lease) {
                Ok(()) => return,
                Err(error) => error.0,
            };
        }
        let connection = lease.expect("only leases are queued for handoff");
        state.in_use -= 1;
        let drained = state.in_use == 0;
        if state.closed {
            drop(state);
            (self.closer)(connection);
        }
        else {
            state.idle.push(connection);
        }
        if drained {
            self.drained.notify_all();
        }
    }

    /// Shuts a connection down for good and frees its slot.
    pub fn discard(&self, connection: C) {
        let mut state = self.lock();
        state.in_use -= 1;
        if state.in_use == 0 {
            self.drained.notify_all();
        }
        drop(state);
        (self.closer)(connection);
    }

    /// Closes the pool: queued waiters are rejected, idle connections shut
    /// down, and the call blocks until every outstanding lease is returned.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        while let Some((_, handoff)) = state.waiters.pop_front() {
            let _ = handoff.send(Err(PoolError::Closed));
        }
        let idle = std::mem::take(&mut state.idle);
        drop(state);
        for connection in idle {
            (self.closer)(connection);
        }
        let mut state = self.lock();
        while state.in_use > 0 {
            state = self.drained.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Current leasing counters.
    pub fn stats(&self) -> PoolStats {
        let state = self.lock();
        PoolStats { in_use: state.in_use, idle: state.idle.len(), waiting: state.waiters.len() }
    }

    /// Whether [`Self::close`] has been called.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn lock(&self) -> MutexGuard<'_, State<C>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    struct MockConnection {
        live: Arc<AtomicUsize>,
    }

    impl Drop for MockConnection {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct Harness {
        pool: Pool<MockConnection>,
        dials: Arc<AtomicUsize>,
        live: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }

    fn harness(limits: PoolLimits) -> Harness {
        let dials = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let (connector_dials, connector_live, connector_fail) = (Arc::clone(&dials), Arc::clone(&live), Arc::clone(&fail));
        let pool = Pool::new(
            limits,
            move || {
                if connector_fail.load(Ordering::SeqCst) {
                    return Err(PostgresError::Io("dial refused".to_string()));
                }
                connector_dials.fetch_add(1, Ordering::SeqCst);
                connector_live.fetch_add(1, Ordering::SeqCst);
                Ok(MockConnection { live: Arc::clone(&connector_live) })
            },
            move |connection| drop(connection),
        );
        Harness { pool, dials, live, fail }
    }

    fn wait_for(predicate: impl Fn() -> bool) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition never became true");
    }

    #[test]
    fn dial_failure_frees_the_reserved_slot() {
        let harness = harness(PoolLimits::default());
        harness.fail.store(true, Ordering::SeqCst);
        assert!(matches!(harness.pool.acquire(), Err(PoolError::Dial(_))));
        harness.fail.store(false, Ordering::SeqCst);
        let lease = harness.pool.acquire().expect("slot was freed by the failed dial");
        assert_eq!(harness.pool.stats().in_use, 1);
        harness.pool.discard(lease);
        harness.pool.close();
    }

    #[test]
    fn released_connections_are_reused_before_dialling() {
        let harness = harness(PoolLimits::default());
        let lease = harness.pool.acquire().expect("first lease dials");
        harness.pool.release(lease);
        assert_eq!(harness.pool.stats().idle, 1);
        let lease = harness.pool.acquire().expect("second lease reuses the idle one");
        assert_eq!(harness.dials.load(Ordering::SeqCst), 1);
        harness.pool.release(lease);
        harness.pool.close();
    }

    #[test]
    fn saturation_refuses_more_waiters_than_max_pending() {
        let harness = Arc::new(harness_with(PoolLimits { max: 1, max_pending: 1, acquire_timeout: Some(Duration::from_millis(400)) }));
        let lease = harness.pool.acquire().expect("the cap dials one lease");
        let queued = {
            let harness = Arc::clone(&harness);
            std::thread::spawn(move || {
                let lease = harness.pool.acquire()?;
                harness.pool.discard(lease);
                Ok::<(), PoolError>(())
            })
        };
        wait_for(|| harness.pool.stats().waiting == 1);
        assert!(matches!(harness.pool.acquire(), Err(PoolError::Saturated)));
        harness.pool.release(lease);
        queued.join().expect("queued thread").expect("handoff succeeded");
        harness.pool.close();
    }

    #[test]
    fn acquire_timeout_drops_the_waiter_and_frees_the_admission_slot() {
        let harness = Arc::new(harness_with(PoolLimits { max: 1, max_pending: 1, acquire_timeout: Some(Duration::from_millis(40)) }));
        let lease = harness.pool.acquire().expect("the cap dials one lease");
        let timed_out = {
            let harness = Arc::clone(&harness);
            std::thread::spawn(move || harness.pool.acquire())
        };
        assert!(matches!(timed_out.join().expect("timed out thread"), Err(PoolError::AcquireTimeout)));
        assert_eq!(harness.pool.stats().waiting, 0);
        // The slot is free again: a new waiter may queue, and releasing the
        // only lease parks it idle instead of handing it to a ghost waiter.
        harness.pool.release(lease);
        assert_eq!(harness.pool.stats().idle, 1);
        harness.pool.close();
    }

    #[test]
    fn release_hands_the_lease_to_the_oldest_waiter_without_dialling() {
        let harness = Arc::new(harness_with(PoolLimits { max: 1, max_pending: 4, acquire_timeout: None }));
        let lease = harness.pool.acquire().expect("the cap dials one lease");
        let queued = {
            let harness = Arc::clone(&harness);
            std::thread::spawn(move || {
                let lease = harness.pool.acquire()?;
                harness.pool.release(lease);
                Ok::<(), PoolError>(())
            })
        };
        wait_for(|| harness.pool.stats().waiting == 1);
        harness.pool.release(lease);
        queued.join().expect("queued thread").expect("handoff succeeded");
        assert_eq!(harness.dials.load(Ordering::SeqCst), 1);
        assert_eq!(harness.pool.stats().idle, 1);
        assert_eq!(harness.pool.stats().in_use, 0);
        harness.pool.close();
    }

    #[test]
    fn close_rejects_waiters_and_drains_outstanding_leases() {
        let harness = Arc::new(harness_with(PoolLimits { max: 1, max_pending: 4, acquire_timeout: None }));
        let lease = harness.pool.acquire().expect("the cap dials one lease");
        let queued = {
            let harness = Arc::clone(&harness);
            std::thread::spawn(move || harness.pool.acquire())
        };
        wait_for(|| harness.pool.stats().waiting == 1);
        let closing = {
            let harness = Arc::clone(&harness);
            std::thread::spawn(move || harness.pool.close())
        };
        wait_for(|| harness.pool.is_closed());
        assert!(matches!(queued.join().expect("rejected thread"), Err(PoolError::Closed)));
        harness.pool.release(lease);
        closing.join().expect("close returns once the lease is back");
        wait_for(|| harness.live.load(Ordering::SeqCst) == 0);
        assert!(matches!(harness.pool.acquire(), Err(PoolError::Closed)));
    }

    #[test]
    fn discard_shuts_the_connection_down_and_frees_the_slot() {
        let harness = harness(PoolLimits::default());
        let lease = harness.pool.acquire().expect("first lease dials");
        harness.pool.discard(lease);
        assert_eq!(harness.live.load(Ordering::SeqCst), 0);
        assert_eq!(harness.pool.stats().in_use, 0);
        harness.pool.close();
    }

    fn harness_with(limits: PoolLimits) -> Harness {
        harness(limits)
    }
}
