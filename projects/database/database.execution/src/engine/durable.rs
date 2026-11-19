//! Durable main-image session: one in-memory connection over a host-owned store.
//!
//! This is the shared publication state machine behind hosts that keep the
//! whole database image in a single external slot (browser OPFS, a project
//! directory, any [`MainImagePublisher`]). It owns exactly the decisions the
//! host must not re-implement: when a mutation has to be published, what a
//! failed publication leaves behind, and what `close` owes before it closes.
//!
//! The rules mirror the SQLite session contract in
//! [`crate::sqlite::session`]: a successful write publishes immediately and
//! only then clears the dirty flag; a failed publication propagates the error
//! and keeps the session open and dirty, so the caller can fix the environment
//! and retry; `close` publishes before it closes and stays open on failure.
//! Physical durability belongs to the publisher, never to this module.

use crate::engine::{Connection, Error, Result};

/// Owner of the durable main image behind a [`DurableSession`].
///
/// The publisher is the only path to the stored image. Implementations must
/// return [`None`] from [`Self::read_main`] only when nothing has ever been
/// published (never for a zero-length placeholder), and must accept back
/// exactly the bytes a previous [`Self::publish_main`] stored.
pub trait MainImagePublisher: Send {
    /// Read the current durable main image, or `None` when nothing is stored.
    fn read_main(&mut self) -> Result<Option<Vec<u8>>>;

    /// Replace the durable main image with `image`.
    ///
    /// A failed publication must leave the previous image intact so a retry
    /// or a later `close` can still publish the same bytes.
    fn publish_main(&mut self, image: &[u8]) -> Result<()>;

    /// Number of successful publications through this publisher so far.
    fn generation(&mut self) -> u64;
}

/// An in-memory [`Connection`] whose durable image lives in a [`MainImagePublisher`].
///
/// The session holds the only handle to the connection and publishes the whole
/// main image after every successful write. Reads go through
/// [`Self::connection`]; mutations must go through [`Self::write`] or
/// [`Self::conditional_write`] so the dirty flag and the publication stay in
/// lockstep with the image.
pub struct DurableSession {
    conn: Connection,
    publisher: Box<dyn MainImagePublisher>,
    dirty: bool,
    closed: bool,
}

impl DurableSession {
    /// Open a session, hydrating the connection from the stored image if one exists.
    pub fn open(mut publisher: Box<dyn MainImagePublisher>) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        if let Some(image) = publisher.read_main()? {
            conn.load_main_snapshot(&image)?;
        }
        Ok(Self { conn, publisher, dirty: false, closed: false })
    }

    /// Create a session from `image`, publishing it before the connection hydrates.
    ///
    /// Publication happens first so a caller who drops the session without any
    /// write still finds the restored image in the durable store.
    pub fn restore(mut publisher: Box<dyn MainImagePublisher>, image: &[u8]) -> Result<Self> {
        publisher.publish_main(image)?;
        let conn = Connection::open_in_memory()?;
        conn.load_main_snapshot(image)?;
        Ok(Self { conn, publisher, dirty: false, closed: false })
    }

    /// Run one mutation against the connection, then publish the new image.
    ///
    /// The operation result is returned only after a successful publication; a
    /// failed publication propagates the error and leaves the session open and
    /// dirty, so [`Self::publish`] or the next write can retry.
    pub fn write<T>(&mut self, op: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.guard_open()?;
        let value = op(&self.conn)?;
        self.dirty = true;
        self.publish()?;
        Ok(value)
    }

    /// Run one compare-and-set mutation; publish only when the operation says it applied.
    ///
    /// The operation must leave the connection untouched when it returns
    /// `Ok(false)`; only `Ok(true)` marks the image dirty and publishes.
    pub fn conditional_write(&mut self, op: impl FnOnce(&Connection) -> Result<bool>) -> Result<bool> {
        self.guard_open()?;
        let applied = op(&self.conn)?;
        if !applied {
            return Ok(false);
        }
        self.dirty = true;
        self.publish()?;
        Ok(true)
    }

    /// Publish the current image when something is unpublished.
    ///
    /// A no-op while clean. On failure the dirty flag is kept so the next
    /// publish retries the same image.
    pub fn publish(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let image = self.conn.main_snapshot()?;
        self.publisher.publish_main(&image)?;
        self.dirty = false;
        Ok(())
    }

    /// Publish any pending image, then close the session.
    ///
    /// Idempotent after a successful close. A failed publication propagates
    /// the error and the session stays open and dirty, so the caller can retry.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.publish()?;
        self.conn.close();
        self.closed = true;
        Ok(())
    }

    /// Borrow the connection for read-only surfaces, rejecting a closed session.
    pub fn connection(&self) -> Result<&Connection> {
        self.guard_open()?;
        Ok(&self.conn)
    }

    /// Whether the image holds changes that are not in the durable store yet.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether [`Self::close`] has completed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The publisher's count of successful publications.
    pub fn generation(&mut self) -> u64 {
        self.publisher.generation()
    }

    fn guard_open(&self) -> Result<()> {
        if self.closed {
            return Err(Error::SessionClosed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    use super::*;

    /// In-memory publisher whose next publication can be made to fail from
    /// outside the box the session holds.
    struct MockPublisher {
        image: Option<Vec<u8>>,
        generation: u64,
        fail_next: Arc<AtomicBool>,
    }

    impl MockPublisher {
        fn new() -> Self {
            Self { image: None, generation: 0, fail_next: Arc::new(AtomicBool::new(false)) }
        }
    }

    impl MainImagePublisher for MockPublisher {
        fn read_main(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.image.clone())
        }

        fn publish_main(&mut self, image: &[u8]) -> Result<()> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(Error::Unsupported("mock publication failed"));
            }
            self.image = Some(image.to_vec());
            self.generation += 1;
            Ok(())
        }

        fn generation(&mut self) -> u64 {
            self.generation
        }
    }

    /// A publisher that records the order of its calls, shared with the test.
    struct OrderedPublisher {
        inner: MockPublisher,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl MainImagePublisher for OrderedPublisher {
        fn read_main(&mut self) -> Result<Option<Vec<u8>>> {
            self.log.lock().unwrap().push("read".to_owned());
            self.inner.read_main()
        }

        fn publish_main(&mut self, image: &[u8]) -> Result<()> {
            self.log.lock().unwrap().push("publish".to_owned());
            self.inner.publish_main(image)
        }

        fn generation(&mut self) -> u64 {
            self.inner.generation()
        }
    }

    #[test]
    fn write_publishes_the_image_and_clears_dirty() {
        let mut session = DurableSession::open(Box::new(MockPublisher::new())).expect("open");
        session.write(|conn| conn.put("k", b"v")).expect("write");
        assert!(!session.is_dirty());
        assert_eq!(session.generation(), 1);
        assert_eq!(session.connection().expect("open").get("k").expect("get"), Some(b"v".to_vec()));
    }

    #[test]
    fn failed_publication_keeps_session_open_and_dirty_and_retry_succeeds() {
        let publisher = MockPublisher::new();
        let fail_next = Arc::clone(&publisher.fail_next);
        fail_next.store(true, Ordering::SeqCst);
        let mut session = DurableSession::open(Box::new(publisher)).expect("open");
        let error = session.write(|conn| conn.put("k", b"v")).expect_err("publication must fail");
        assert!(matches!(error, Error::Unsupported(_)));
        assert!(session.is_dirty());
        assert!(!session.is_closed());
        // The mutation is live in the connection even though the store is stale.
        assert_eq!(session.connection().expect("open").get("k").expect("get"), Some(b"v".to_vec()));
        session.publish().expect("retry");
        assert!(!session.is_dirty());
        assert_eq!(session.generation(), 1);
    }

    #[test]
    fn conditional_write_publishes_only_when_applied() {
        let mut session = DurableSession::open(Box::new(MockPublisher::new())).expect("open");
        let applied = session.conditional_write(|conn| Ok(conn.get("absent").expect("get").is_some())).expect("cas");
        assert!(!applied);
        assert!(!session.is_dirty());
        assert_eq!(session.generation(), 0);
        let applied = session
            .conditional_write(|conn| {
                conn.put("k", b"v")?;
                Ok(true)
            })
            .expect("cas");
        assert!(applied);
        assert_eq!(session.generation(), 1);
    }

    #[test]
    fn close_publishes_pending_image_then_stays_idempotent() {
        let publisher = MockPublisher::new();
        let fail_next = Arc::clone(&publisher.fail_next);
        let mut session = DurableSession::open(Box::new(publisher)).expect("open");
        // Fail the write's own publication, then retry it away so the session
        // is clean again.
        fail_next.store(true, Ordering::SeqCst);
        assert!(session.write(|conn| conn.put("k", b"v")).is_err());
        session.publish().expect("retry");
        // A second change stays unpublished when its write fails; close owes
        // that publication.
        fail_next.store(true, Ordering::SeqCst);
        assert!(session.write(|conn| conn.put("k2", b"v2")).is_err());
        assert!(session.is_dirty());
        session.close().expect("close publishes");
        assert!(session.is_closed());
        assert!(!session.is_dirty());
        assert_eq!(session.generation(), 2);
        session.close().expect("second close is a no-op");
        assert_eq!(session.generation(), 2);
    }

    #[test]
    fn close_failure_keeps_session_open_and_dirty_for_retry() {
        let publisher = MockPublisher::new();
        let fail_next = Arc::clone(&publisher.fail_next);
        let mut session = DurableSession::open(Box::new(publisher)).expect("open");
        session.write(|conn| conn.put("k", b"v")).expect("write");
        fail_next.store(true, Ordering::SeqCst);
        assert!(session.write(|conn| conn.put("k2", b"v2")).is_err());
        fail_next.store(true, Ordering::SeqCst);
        let error = session.close().expect_err("close must propagate the publication failure");
        assert!(matches!(error, Error::Unsupported(_)));
        assert!(!session.is_closed());
        assert!(session.is_dirty());
        session.close().expect("retry closes");
        assert!(session.is_closed());
        assert_eq!(session.generation(), 2);
    }

    #[test]
    fn operations_on_a_closed_session_are_rejected() {
        let mut session = DurableSession::open(Box::new(MockPublisher::new())).expect("open");
        session.close().expect("close");
        assert!(matches!(session.write(|conn| conn.put("k", b"v")), Err(Error::SessionClosed)));
        assert!(matches!(session.conditional_write(|_conn| Ok(true)), Err(Error::SessionClosed)));
        assert!(matches!(session.connection(), Err(Error::SessionClosed)));
    }

    #[test]
    fn open_hydrates_from_the_stored_image_and_restore_publishes_before_hydrating() {
        let mut session = DurableSession::open(Box::new(MockPublisher::new())).expect("open");
        session.write(|conn| conn.put("k", b"v")).expect("write");
        let image = session.connection().expect("conn").main_snapshot().expect("snapshot");
        session.close().expect("close");

        // Reopen over a publisher holding the image: the record is already there.
        let stored = MockPublisher { image: Some(image.clone()), generation: 7, fail_next: Arc::new(AtomicBool::new(false)) };
        let mut reopened = DurableSession::open(Box::new(stored)).expect("reopen");
        assert_eq!(reopened.connection().expect("conn").get("k").expect("get"), Some(b"v".to_vec()));
        assert!(!reopened.is_dirty());
        assert_eq!(reopened.generation(), 7);

        // Restore publishes before the connection hydrates.
        let log = Arc::new(Mutex::new(Vec::new()));
        let publisher = OrderedPublisher { inner: MockPublisher::new(), log: Arc::clone(&log) };
        let restored = DurableSession::restore(Box::new(publisher), &image).expect("restore");
        assert_eq!(restored.connection().expect("conn").get("k").expect("get"), Some(b"v".to_vec()));
        assert_eq!(log.lock().unwrap().as_slice(), ["publish".to_owned()]);
    }

    #[test]
    fn publish_on_a_clean_session_is_a_no_op() {
        let mut session = DurableSession::open(Box::new(MockPublisher::new())).expect("open");
        session.publish().expect("no-op");
        assert_eq!(session.generation(), 0);
    }
}
