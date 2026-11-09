//! `NSPasteboard.general`. There is no change notification, so we poll
//! `changeCount` (cheap integer read) every 300 ms. Writes we make ourselves
//! are recognised by their resulting `changeCount` and not reported.

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use linker_platform::hash::to_png;
use linker_platform::{Cap, ClipboardChange, ClipboardContent, Unsupported};
use objc2::msg_send;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, Bool};
use tokio::sync::mpsc;

use crate::objc::{as_nsstring, class, ns, nsarray_items, nsdata, nsdata_to_vec};

const T_STRING: &str = "public.utf8-plain-text";
const T_HTML: &str = "public.html";
const T_PNG: &str = "public.png";
const T_TIFF: &str = "public.tiff";
const T_FILE_URL: &str = "public.file-url";
/// nspasteboard.org markers used by password managers.
const T_CONCEALED: &str = "org.nspasteboard.ConcealedType";
const T_TRANSIENT: &str = "org.nspasteboard.TransientType";

pub struct Pasteboard {
    /// changeCount right after our own last write.
    own_change: AtomicIsize,
}

fn general() -> anyhow::Result<Retained<AnyObject>> {
    let cls = class("NSPasteboard")?;
    // SAFETY: +generalPasteboard is always available.
    let pb: Option<Retained<AnyObject>> = unsafe { msg_send![cls, generalPasteboard] };
    pb.ok_or_else(|| anyhow::anyhow!("no general pasteboard"))
}

fn change_count(pb: &AnyObject) -> isize {
    // SAFETY: standard AppKit API.
    unsafe { msg_send![pb, changeCount] }
}

fn types(pb: &AnyObject) -> Vec<String> {
    // SAFETY: standard AppKit API.
    let arr: Option<Retained<AnyObject>> = unsafe { msg_send![pb, types] };
    arr.map(|a| nsarray_items(&a).iter().map(|s| as_nsstring(s)).collect())
        .unwrap_or_default()
}

fn string_for(obj: &AnyObject, ty: &str) -> Option<String> {
    let t = ns(ty);
    // SAFETY: standard AppKit API (NSPasteboard / NSPasteboardItem).
    let s: Option<Retained<AnyObject>> = unsafe { msg_send![obj, stringForType: &*t] };
    s.map(|s| as_nsstring(&s))
}

fn data_for(pb: &AnyObject, ty: &str) -> Option<Vec<u8>> {
    let t = ns(ty);
    // SAFETY: standard AppKit API.
    let d: Option<Retained<AnyObject>> = unsafe { msg_send![pb, dataForType: &*t] };
    d.map(|d| nsdata_to_vec(&d))
}

fn file_urls(pb: &AnyObject) -> Vec<PathBuf> {
    // SAFETY: standard AppKit/Foundation API.
    unsafe {
        let items: Option<Retained<AnyObject>> = msg_send![pb, pasteboardItems];
        let Some(items) = items else { return vec![] };
        let Ok(url_cls) = class("NSURL") else {
            return vec![];
        };
        nsarray_items(&items)
            .iter()
            .filter_map(|item| {
                let s = string_for(item, T_FILE_URL)?;
                let s = ns(&s);
                let url: Option<Retained<AnyObject>> = msg_send![url_cls, URLWithString: &*s];
                let url = url?;
                // Resolve file reference URLs (file:///.file/id=…) to paths.
                let url: Retained<AnyObject> = msg_send![&*url, filePathURL];
                let path: Option<Retained<AnyObject>> = msg_send![&*url, path];
                path.map(|p| PathBuf::from(as_nsstring(&p)))
            })
            .collect()
    }
}

impl Pasteboard {
    pub fn new() -> Pasteboard {
        Pasteboard {
            own_change: AtomicIsize::new(-1),
        }
    }

    fn read(&self, pb: &AnyObject) -> Option<ClipboardChange> {
        let t = types(pb);
        let has = |x: &str| t.iter().any(|y| y == x);
        let concealed = has(T_CONCEALED) || has(T_TRANSIENT);
        let content = if has(T_FILE_URL) {
            let paths = file_urls(pb);
            if paths.is_empty() {
                return None;
            }
            ClipboardContent::Files { paths }
        } else if has(T_STRING) {
            let text = string_for(pb, T_STRING)?;
            let html = if has(T_HTML) {
                string_for(pb, T_HTML)
            } else {
                None
            };
            ClipboardContent::Text { text, html }
        } else if has(T_PNG) {
            ClipboardContent::Image {
                png: data_for(pb, T_PNG)?,
            }
        } else if has(T_TIFF) {
            let tiff = data_for(pb, T_TIFF)?;
            ClipboardContent::Image {
                png: to_png(&tiff).ok()?,
            }
        } else {
            return None;
        };
        Some(ClipboardChange { content, concealed })
    }

    pub fn write(&self, content: ClipboardContent) -> anyhow::Result<()> {
        autoreleasepool(|_| {
            let pb = general()?;
            // SAFETY: standard AppKit API.
            unsafe {
                let _: isize = msg_send![&*pb, clearContents];
                let ok: Bool = match &content {
                    ClipboardContent::Text { text, html } => {
                        let t = ns(T_STRING);
                        let v = ns(text);
                        let mut ok: Bool = msg_send![&*pb, setString: &*v, forType: &*t];
                        if let Some(h) = html {
                            let t = ns(T_HTML);
                            let v = ns(h);
                            ok = msg_send![&*pb, setString: &*v, forType: &*t];
                        }
                        ok
                    }
                    ClipboardContent::Image { png } => {
                        let t = ns(T_PNG);
                        let d = nsdata(png)?;
                        msg_send![&*pb, setData: &*d, forType: &*t]
                    }
                    ClipboardContent::Files { paths } => {
                        let url_cls = class("NSURL")?;
                        let arr_cls = class("NSMutableArray")?;
                        let arr: Retained<AnyObject> = msg_send![arr_cls, new];
                        for p in paths {
                            let s = ns(&p.to_string_lossy());
                            let url: Retained<AnyObject> = msg_send![url_cls, fileURLWithPath: &*s];
                            let _: () = msg_send![&*arr, addObject: &*url];
                        }
                        msg_send![&*pb, writeObjects: &*arr]
                    }
                };
                self.own_change.store(change_count(&pb), Ordering::SeqCst);
                if !ok.as_bool() {
                    anyhow::bail!("NSPasteboard refused the data");
                }
            }
            Ok(())
        })
    }
}

pub fn watch(pbs: Arc<Pasteboard>, tx: mpsc::Sender<ClipboardChange>) -> Cap<()> {
    general().map_err(Unsupported::from)?;
    std::thread::Builder::new()
        .name("linker-pasteboard".into())
        .spawn(move || {
            let mut last = autoreleasepool(|_| general().map(|pb| change_count(&pb)).unwrap_or(0));
            tracing::info!("pasteboard watcher active (changeCount polling)");
            loop {
                std::thread::sleep(Duration::from_millis(300));
                if tx.is_closed() {
                    return;
                }
                let change = autoreleasepool(|_| {
                    let pb = general().ok()?;
                    let cc = change_count(&pb);
                    if cc == last {
                        return None;
                    }
                    last = cc;
                    if cc == pbs.own_change.load(Ordering::SeqCst) {
                        return None; // our own write
                    }
                    pbs.read(&pb)
                });
                if let Some(c) = change {
                    if tx.blocking_send(c).is_err() {
                        return;
                    }
                }
            }
        })
        .map_err(|e| Unsupported::new(e.to_string()))?;
    Ok(())
}
