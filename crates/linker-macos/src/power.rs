//! Sleep / wake via `NSWorkspace` notifications, which are delivered on the
//! main thread's run loop. `run_with_main_loop` runs the daemon on a worker
//! thread and keeps the main thread in `CFRunLoopRun`.

use std::ptr::NonNull;
use std::sync::Mutex;

use block2::RcBlock;
use core_foundation::runloop::CFRunLoop;
use linker_platform::{Cap, PowerEvent};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use tokio::sync::mpsc;

use crate::objc::{class, ns};

static SUBSCRIBERS: Mutex<Vec<mpsc::Sender<PowerEvent>>> = Mutex::new(Vec::new());
static OBSERVING: Mutex<bool> = Mutex::new(false);

fn broadcast(ev: PowerEvent) {
    tracing::info!(?ev, "NSWorkspace power event");
    let mut subs = SUBSCRIBERS.lock().expect("power lock");
    subs.retain(|tx| !matches!(tx.try_send(ev), Err(mpsc::error::TrySendError::Closed(_))));
}

pub fn subscribe(tx: mpsc::Sender<PowerEvent>) -> Cap<()> {
    if !*OBSERVING.lock().expect("power lock") {
        return Err(linker_platform::Unsupported::new(
            "main run loop not running",
        ));
    }
    SUBSCRIBERS.lock().expect("power lock").push(tx);
    Ok(())
}

/// Register NSWorkspace observers. Must be called on the main thread.
fn install_observers() -> anyhow::Result<()> {
    let ws_cls = class("NSWorkspace")?;
    // SAFETY: standard AppKit API, called on the main thread.
    unsafe {
        let ws: Retained<AnyObject> = msg_send![ws_cls, sharedWorkspace];
        let nc: Retained<AnyObject> = msg_send![&*ws, notificationCenter];
        for (name, ev) in [
            ("NSWorkspaceWillSleepNotification", PowerEvent::Sleeping),
            ("NSWorkspaceDidWakeNotification", PowerEvent::Woke),
        ] {
            let block = RcBlock::new(move |_n: NonNull<AnyObject>| broadcast(ev));
            let name = ns(name);
            let token: Option<Retained<AnyObject>> = msg_send![
                &*nc,
                addObserverForName: &*name,
                object: None::<&AnyObject>,
                queue: None::<&AnyObject>,
                usingBlock: &*block
            ];
            // Observers live for the whole process.
            std::mem::forget(token);
            std::mem::forget(block);
        }
    }
    *OBSERVING.lock().expect("power lock") = true;
    Ok(())
}

/// Run `f` on a worker thread while the main thread services the run loop.
/// Returns `f`'s result once it finishes.
pub fn run_with_main_loop<F>(f: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    if let Err(e) = install_observers() {
        tracing::warn!("sleep/wake notifications unavailable: {e:#}");
    }
    let result = std::sync::Arc::new(Mutex::new(None));
    let r2 = result.clone();
    let main = CFRunLoop::get_main();
    let main_ref = SendRunLoop(main.clone());
    std::thread::Builder::new()
        .name("linkerd-main".into())
        .spawn(move || {
            let res = f();
            *r2.lock().expect("result lock") = Some(res);
            let main_ref = main_ref;
            main_ref.0.stop();
        })?;
    CFRunLoop::run_current();
    let res = result.lock().expect("result lock").take();
    res.unwrap_or(Ok(()))
}

struct SendRunLoop(CFRunLoop);
// SAFETY: CFRunLoopStop is documented as thread-safe.
unsafe impl Send for SendRunLoop {}
