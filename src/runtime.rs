//! Shared Tokio runtime and tasks bound to their owners.

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn shared() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("tokio-rt-fern-topbar-pool")
            .build()
            .expect("failed to create the async runtime")
    })
}

pub fn spawn<T>(future: impl Future<Output = T> + Send + 'static) -> JoinHandle<T>
where
    T: Send + 'static,
{
    shared().spawn(future)
}

/// Owns a background task and aborts it when its feature or subscription is dropped.
///
/// `_task` and `_forwarder` fields are lifetime guards even when never read.
/// Dropping a plain Tokio `JoinHandle` only detaches the task; retaining this
/// wrapper ties cancellation to the UI/service owner instead.
pub struct Task {
    handle: JoinHandle<()>,
}

impl Task {
    pub fn spawn(future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            handle: spawn(future),
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
