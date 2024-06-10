use std::sync::atomic::{AtomicUsize, Ordering};
use thread_local::ThreadLocal;
use tokio::sync;

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
    pub fn hits(&self) -> usize {
        self.get().hits()
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
    hits: AtomicUsize,
}

impl TestSyncMarkerInner {
    fn new() -> Self {
        let (tx, rx) = sync::mpsc::channel(1);
        Self {
            tx,
            rx: sync::Mutex::new(rx),
            hits: AtomicUsize::new(0),
        }
    }

    /// Mark that code has encountered this location.
    pub fn mark(&self) {
        // If we cannot send, don't block the program.
        let _ = self.tx.try_send(());
        self.hits.fetch_add(1, Ordering::SeqCst);
    }

    /// Wait until code has encountered this location.
    pub async fn sync(&self) {
        self.rx.lock().await.recv().await.unwrap();
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}
