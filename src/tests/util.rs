use once_cell::sync::Lazy;
use tokio::sync;

pub struct TestSyncMarker {
    state: Lazy<TestSyncMarkerInner>,
}

impl TestSyncMarker {
    pub const fn new() -> Self {
        Self {
            state: Lazy::new(TestSyncMarkerInner::new),
        }
    }

    /// Mark that code has encountered this location.
    pub fn mark(&self) {
        // If we cannot send, don't block the program.
        let _ = self.state.tx.try_send(());
    }

    /// Wait until code has encountered this location.
    pub async fn sync(&self) {
        self.state.rx.lock().await.recv().await.unwrap();
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
}
