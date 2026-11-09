//! Feature modules. Each runs as an independent task, reacts to link
//! up/down via `Shared::watch_link`, and degrades to "unsupported" (keeping
//! the rest of the daemon running) when its OS facility is missing.

pub mod camera;
pub mod clipboard;
pub mod kvm;
pub mod notify;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::link::RouterRx;
use crate::Shared;

pub fn spawn_all(shared: Arc<Shared>, rx: RouterRx) {
    let RouterRx {
        clipboard,
        clipboard_bulk,
        notify,
        kvm,
        camera,
        camera_dgram,
    } = rx;
    tokio::spawn(clipboard::run(shared.clone(), clipboard, clipboard_bulk));
    tokio::spawn(notify::run(shared.clone(), notify));
    tokio::spawn(kvm::run(shared.clone(), kvm));
    tokio::spawn(camera::run(shared, camera, camera_dgram));
}

/// Keep a router channel drained when its feature is disabled, so the
/// session reader never blocks on it.
pub(crate) fn drain<T: Send + 'static>(mut rx: mpsc::Receiver<T>) {
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
}
