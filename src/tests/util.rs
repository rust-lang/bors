use thread_local::ThreadLocal;
use tokio::sync;

/// This struct serves for waiting for certain async (and unsynchronized) events to happen in tests.
/// The test should start an async operation and then call `sync().await` to wait until it's
/// finished. The expectation is that the operation will eventually call `mark()` to unblock the
/// test.
pub struct TestSyncMarker {
    inner: ThreadLocal<TestSyncMarkerInner>,
}

impl TestSyncMarker {
    pub const fn new() -> Self {
        Self {
            inner: ThreadLocal::new(),
        }
    }

    pub fn mark(&self) {
        self.get().mark();
    }
    pub async fn sync(&self) {
        self.get().sync().await;
    }

    fn get(&self) -> &TestSyncMarkerInner {
        self.inner
            .get_or_try(|| Ok::<TestSyncMarkerInner, ()>(TestSyncMarkerInner::new()))
            .unwrap()
    }
}

struct TestSyncMarkerInner {
    rx: sync::Mutex<sync::mpsc::Receiver<()>>,
    tx: sync::mpsc::Sender<()>,
}

impl TestSyncMarkerInner {
    fn new() -> Self {
        let (tx, rx) = sync::mpsc::channel(1);
        Self {
            tx,
            rx: sync::Mutex::new(rx),
        }
    }

    /// Mark that code has encountered this location.
    pub fn mark(&self) {
        // If we cannot send, don't block the program.
        let _ = self.tx.try_send(());
    }

    /// Wait until code has encountered this location.
    pub async fn sync(&self) {
        self.rx.lock().await.recv().await.unwrap();
    }
}
