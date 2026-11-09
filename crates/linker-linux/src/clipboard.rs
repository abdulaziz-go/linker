//! Clipboard via `arboard`.
//!
//! On GNOME Wayland, Mutter bridges the Wayland clipboard to Xwayland's
//! CLIPBOARD selection, so the X11 path works for both session types; we get
//! instant change notification from the XFixes extension. On compositors
//! with `wlr-data-control` (sway, KDE) arboard talks Wayland directly and we
//! poll for changes. Without any display the watcher waits and retries, so
//! the feature comes up automatically once the user logs in.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use linker_platform::hash::rgba_to_png;
use linker_platform::{Cap, ClipboardChange, ClipboardContent};
use tokio::sync::mpsc;

use crate::session::{self, DisplayServer};

pub struct Clip {
    /// arboard serves our selection only while a Clipboard instance lives,
    /// so we keep one for the life of the daemon.
    cb: Mutex<Option<arboard::Clipboard>>,
}

impl Clip {
    pub fn new() -> Clip {
        Clip {
            cb: Mutex::new(None),
        }
    }

    fn with<R>(
        &self,
        f: impl FnOnce(&mut arboard::Clipboard) -> anyhow::Result<R>,
    ) -> anyhow::Result<R> {
        let mut g = self.cb.lock().expect("clipboard lock");
        if g.is_none() {
            session::refresh_env();
            *g = Some(
                arboard::Clipboard::new()
                    .context("no clipboard (graphical session not available?)")?,
            );
        }
        let r = f(g.as_mut().expect("just set"));
        if r.is_err() {
            // Session may have ended (logout); reconnect next time.
            *g = None;
        }
        r
    }

    pub fn read(&self) -> Option<ClipboardContent> {
        self.with(|cb| {
            if let Ok(files) = cb.get().file_list() {
                if !files.is_empty() {
                    return Ok(Some(ClipboardContent::Files { paths: files }));
                }
            }
            if let Ok(text) = cb.get().text() {
                if !text.is_empty() {
                    let html = cb.get().html().ok();
                    return Ok(Some(ClipboardContent::Text { text, html }));
                }
            }
            if let Ok(img) = cb.get().image() {
                let png = rgba_to_png(img.width as u32, img.height as u32, &img.bytes)?;
                return Ok(Some(ClipboardContent::Image { png }));
            }
            Ok(None)
        })
        .ok()
        .flatten()
    }

    pub fn write(&self, content: ClipboardContent) -> anyhow::Result<()> {
        self.with(|cb| {
            match content {
                ClipboardContent::Text {
                    text,
                    html: Some(html),
                } => cb.set().html(html, Some(text))?,
                ClipboardContent::Text { text, html: None } => cb.set().text(text)?,
                ClipboardContent::Image { png } => {
                    let img = image::load_from_memory(&png)?.to_rgba8();
                    let (w, h) = img.dimensions();
                    cb.set().image(arboard::ImageData {
                        width: w as usize,
                        height: h as usize,
                        bytes: img.into_raw().into(),
                    })?
                }
                ClipboardContent::Files { paths } => cb.set().file_list(&paths)?,
            }
            Ok(())
        })
    }
}

pub fn watch(clip: Arc<Clip>, tx: mpsc::Sender<ClipboardChange>) -> Cap<()> {
    std::thread::Builder::new()
        .name("linker-clipboard".into())
        .spawn(move || loop {
            session::refresh_env();
            let res = match session::display_server() {
                DisplayServer::None => Err(anyhow::anyhow!("no graphical session yet")),
                ds => match xfixes_loop(&clip, &tx) {
                    Ok(()) => return, // receiver gone
                    Err(e) if ds == DisplayServer::Wayland => {
                        tracing::info!("XFixes unavailable ({e:#}); polling Wayland clipboard");
                        poll_loop(&clip, &tx)
                    }
                    Err(e) => Err(e),
                },
            };
            match res {
                Ok(()) => return,
                Err(e) => {
                    tracing::debug!("clipboard watcher: {e:#}; retrying in 5s");
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        })
        .map_err(|e| linker_platform::Unsupported::new(e.to_string()))?;
    Ok(())
}

fn emit(
    clip: &Clip,
    tx: &mpsc::Sender<ClipboardChange>,
    last: &mut Option<ClipboardContent>,
) -> bool {
    // Let the owner finish publishing all targets.
    std::thread::sleep(Duration::from_millis(30));
    if let Some(c) = clip.read() {
        if last.as_ref() != Some(&c) {
            *last = Some(c.clone());
            return tx
                .blocking_send(ClipboardChange {
                    content: c,
                    concealed: false,
                })
                .is_ok();
        }
    }
    true
}

/// Block on XFixes selection-owner-change events for CLIPBOARD.
fn xfixes_loop(clip: &Clip, tx: &mpsc::Sender<ClipboardChange>) -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
    use x11rb::protocol::Event;

    let (conn, screen) = x11rb::connect(None).context("connecting to X server")?;
    conn.xfixes_query_version(5, 0)?
        .reply()
        .context("XFixes not available")?;
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id()?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    conn.xfixes_select_selection_input(
        win,
        clipboard,
        SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    conn.flush()?;
    tracing::info!("clipboard watcher active (XFixes)");
    let mut last = clip.read();
    loop {
        let ev = conn.wait_for_event().context("X connection lost")?;
        if let Event::XfixesSelectionNotify(e) = ev {
            if e.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER
                && !emit(clip, tx, &mut last)
            {
                return Ok(());
            }
        }
    }
}

fn poll_loop(clip: &Clip, tx: &mpsc::Sender<ClipboardChange>) -> anyhow::Result<()> {
    tracing::info!("clipboard watcher active (polling)");
    let mut last = clip.read();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if tx.is_closed() {
            return Ok(());
        }
        if !emit(clip, tx, &mut last) {
            return Ok(());
        }
    }
}
