//! Universal Clipboard (text, HTML, PNG images, files; both directions).
//!
//! * Small text (≤ `inline_threshold`) goes inline as `ClipText` on the
//!   control stream.
//! * Images, files and large text are announced with `ClipOffer`; the peer
//!   pulls them on a dedicated QUIC stream (`ClipPull` → `ClipBlob` header +
//!   raw bytes), so a big payload never stalls input or notifications.
//! * Echo prevention: each payload carries a canonical content hash; the
//!   receiver records it as "just applied" and its watcher ignores the
//!   matching change.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use linker_platform::echo::EchoGuard;
use linker_platform::hash::content_hash;
use linker_platform::{now_ms, ClipboardChange, ClipboardContent};
use linker_proto::{
    BlobItem, ClipBlob, ClipItemMeta, ClipKind, ClipOffer, ClipPull, ClipText, ContentHash,
    Feature, Message,
};
use quinn::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::link::{read_msg, write_msg};
use crate::Shared;

const ZSTD_MAX_ITEM: u64 = 64 * 1024 * 1024;

struct Outgoing {
    id: u64,
    content: ClipboardContent,
}

struct State {
    guard: Mutex<EchoGuard<ContentHash>>,
    /// The offer we are currently serving to the peer.
    offer: Mutex<Option<Outgoing>>,
    /// Latest offer id received from the peer (older pulls are discarded).
    latest_incoming: Mutex<u64>,
}

pub async fn run(
    shared: Arc<Shared>,
    mut rx: mpsc::Receiver<Message>,
    mut bulk_rx: mpsc::Receiver<(SendStream, RecvStream)>,
) {
    let cfg = shared.cfg.clipboard.clone();
    if !cfg.enabled {
        shared.status.feature("clipboard", "disabled");
        super::drain(rx);
        super::drain(bulk_rx);
        return;
    }

    let (tx, mut changes) = mpsc::channel::<ClipboardChange>(16);
    match shared.backend.clipboard_watch(tx) {
        Ok(()) => shared.status.feature("clipboard", "active"),
        Err(e) => shared
            .status
            .feature("clipboard", format!("receive-only ({})", e.0)),
    }

    let st = Arc::new(State {
        guard: Mutex::new(EchoGuard::new(Duration::from_secs(10))),
        offer: Mutex::new(None),
        latest_incoming: Mutex::new(0),
    });
    let mut next_id = now_ms(); // unique across restarts
    let mut last_sent: Option<ContentHash> = None;

    loop {
        tokio::select! {
            Some(change) = changes.recv() => {
                if change.concealed && cfg.skip_concealed {
                    tracing::debug!("skipping concealed clipboard content");
                    continue;
                }
                let content = change.content;
                let c2 = content.clone();
                let Ok(hash) = tokio::task::spawn_blocking(move || content_hash(&c2)).await else { continue };
                if st.guard.lock().expect("lock").is_echo(&hash) {
                    tracing::trace!("clipboard change is our own echo");
                    continue;
                }
                if last_sent == Some(hash) {
                    continue;
                }
                let Some(link) = shared.link() else { continue };
                if !link.peer_has(Feature::Clipboard) {
                    continue;
                }
                match &content {
                    ClipboardContent::Text { text, html }
                        if text.len() + html.as_ref().map_or(0, |h| h.len()) <= cfg.inline_threshold =>
                    {
                        link.send(Message::ClipText(ClipText { hash, text: text.clone(), html: html.clone() }));
                        tracing::debug!(bytes = text.len(), "sent clipboard text");
                    }
                    _ => {
                        let (kind, items) = match describe(&content) {
                            Ok(x) => x,
                            Err(e) => { tracing::warn!("clipboard: {e:#}"); continue; }
                        };
                        let total: u64 = items.iter().map(|i| i.size).sum();
                        if total > cfg.max_size {
                            tracing::info!(total, "clipboard payload exceeds max_size; not syncing");
                            continue;
                        }
                        next_id += 1;
                        *st.offer.lock().expect("lock") = Some(Outgoing { id: next_id, content: content.clone() });
                        link.send(Message::ClipOffer(ClipOffer { id: next_id, hash, kind, total_size: total, items }));
                        tracing::debug!(?kind, total, "offered clipboard payload");
                    }
                }
                last_sent = Some(hash);
            }

            Some(msg) = rx.recv() => match msg {
                Message::ClipText(t) => {
                    st.guard.lock().expect("lock").mark_applied(t.hash);
                    last_sent = Some(t.hash);
                    apply(&shared, ClipboardContent::Text { text: t.text, html: t.html }).await;
                }
                Message::ClipOffer(offer) => {
                    if offer.total_size > cfg.max_size {
                        tracing::info!("peer offered {} bytes > max_size; ignoring", offer.total_size);
                        continue;
                    }
                    *st.latest_incoming.lock().expect("lock") = offer.id;
                    last_sent = Some(offer.hash);
                    let shared = shared.clone();
                    let st = st.clone();
                    tokio::spawn(async move {
                        if let Err(e) = pull(&shared, &st, offer).await {
                            tracing::warn!("clipboard pull failed: {e:#}");
                        }
                    });
                }
                _ => {}
            },

            Some((send, recv)) = bulk_rx.recv() => {
                let st = st.clone();
                let compress = cfg.compress;
                tokio::spawn(async move {
                    if let Err(e) = serve(&st, send, recv, compress).await {
                        tracing::debug!("clipboard serve: {e:#}");
                    }
                });
            }
        }
    }
}

async fn apply(shared: &Arc<Shared>, content: ClipboardContent) {
    let backend = shared.backend.clone();
    match tokio::task::spawn_blocking(move || backend.clipboard_write(content)).await {
        Ok(Ok(())) => tracing::debug!("applied clipboard from peer"),
        Ok(Err(e)) => tracing::warn!("writing clipboard failed: {e:#}"),
        Err(e) => tracing::warn!("clipboard writer panicked: {e}"),
    }
}

fn describe(c: &ClipboardContent) -> anyhow::Result<(ClipKind, Vec<ClipItemMeta>)> {
    Ok(match c {
        ClipboardContent::Text { text, .. } => (
            ClipKind::Text,
            vec![ClipItemMeta {
                name: "text".into(),
                size: text.len() as u64,
            }],
        ),
        ClipboardContent::Image { png } => (
            ClipKind::Image,
            vec![ClipItemMeta {
                name: "image.png".into(),
                size: png.len() as u64,
            }],
        ),
        ClipboardContent::Files { paths } => {
            let mut items = Vec::new();
            for p in paths {
                let md = std::fs::metadata(p).with_context(|| format!("stat {}", p.display()))?;
                if !md.is_file() {
                    tracing::info!("clipboard: skipping non-regular file {}", p.display());
                    continue;
                }
                items.push(ClipItemMeta {
                    name: file_name(p),
                    size: md.len(),
                });
            }
            if items.is_empty() {
                bail!("no regular files to send");
            }
            (ClipKind::Files, items)
        }
    })
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

/// Peer pulls our current offer.
async fn serve(
    st: &State,
    mut send: SendStream,
    mut recv: RecvStream,
    compress: bool,
) -> anyhow::Result<()> {
    let id = match read_msg(&mut recv).await? {
        Some(Message::ClipPull(ClipPull { id })) => id,
        _ => bail!("expected ClipPull"),
    };
    let content = {
        let g = st.offer.lock().expect("lock");
        g.as_ref().filter(|o| o.id == id).map(|o| o.content.clone())
    };
    let Some(content) = content else {
        write_msg(
            &mut send,
            &Message::ClipBlob(ClipBlob {
                id,
                available: false,
                items: vec![],
            }),
        )
        .await?;
        send.finish()?;
        return Ok(());
    };

    // Prepare items: (name, Source)
    enum Src {
        Mem(Vec<u8>),
        File(PathBuf),
    }
    let raw: Vec<(String, Src, u64)> = match content {
        ClipboardContent::Text { text, .. } => {
            let n = text.len() as u64;
            vec![("text".into(), Src::Mem(text.into_bytes()), n)]
        }
        ClipboardContent::Image { png } => {
            let n = png.len() as u64;
            vec![("image.png".into(), Src::Mem(png), n)]
        }
        ClipboardContent::Files { paths } => paths
            .into_iter()
            .filter_map(|p| {
                let md = std::fs::metadata(&p).ok()?;
                md.is_file()
                    .then(|| (file_name(&p), Src::File(p), md.len()))
            })
            .collect(),
    };

    // Compress where it pays off (not PNG, not huge files).
    let mut items = Vec::new();
    let mut bodies = Vec::new();
    for (name, src, raw_size) in raw {
        let do_zstd =
            compress && raw_size <= ZSTD_MAX_ITEM && !name.ends_with(".png") && raw_size > 1024;
        if do_zstd {
            let data = match &src {
                Src::Mem(v) => v.clone(),
                Src::File(p) => tokio::fs::read(p).await?,
            };
            let z = tokio::task::spawn_blocking(move || zstd::encode_all(&data[..], 3)).await??;
            items.push(BlobItem {
                name,
                wire_size: z.len() as u64,
                raw_size,
                zstd: true,
            });
            bodies.push(Src::Mem(z));
        } else {
            items.push(BlobItem {
                name,
                wire_size: raw_size,
                raw_size,
                zstd: false,
            });
            bodies.push(src);
        }
    }
    write_msg(
        &mut send,
        &Message::ClipBlob(ClipBlob {
            id,
            available: true,
            items: items.clone(),
        }),
    )
    .await?;
    for (item, body) in items.iter().zip(bodies) {
        match body {
            Src::Mem(v) => send.write_all(&v).await?,
            Src::File(p) => {
                let f = tokio::fs::File::open(&p).await?;
                let mut limited = f.take(item.wire_size);
                tokio::io::copy(&mut limited, &mut send).await?;
            }
        }
    }
    send.finish()?;
    // Wait for the peer to read everything before dropping the stream.
    let _ = recv.read_to_end(16).await;
    Ok(())
}

/// Pull an offer from the peer and apply it to our clipboard.
async fn pull(shared: &Arc<Shared>, st: &State, offer: ClipOffer) -> anyhow::Result<()> {
    let link = shared.link().context("link went away")?;
    let (mut send, mut recv) = link.open_bulk().await?;
    write_msg(&mut send, &Message::ClipPull(ClipPull { id: offer.id })).await?;
    let blob = match read_msg(&mut recv).await? {
        Some(Message::ClipBlob(b)) => b,
        _ => bail!("expected ClipBlob"),
    };
    if !blob.available {
        return Ok(());
    }

    let recv_dir = || -> anyhow::Result<PathBuf> {
        let base = shared
            .cfg
            .clipboard
            .receive_dir
            .clone()
            .or_else(|| dirs::download_dir().map(|d| d.join("Linker")))
            .or_else(|| dirs::home_dir().map(|d| d.join("Linker")))
            .context("no directory for received files")?;
        let dir = base.join(format!("clip-{}", now_ms()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    };

    let mut mem_items: Vec<Vec<u8>> = Vec::new();
    let mut paths = Vec::new();
    let dir = if offer.kind == ClipKind::Files {
        Some(recv_dir()?)
    } else {
        None
    };
    for item in &blob.items {
        if item.raw_size > shared.cfg.clipboard.max_size {
            bail!("item too large");
        }
        match &dir {
            Some(dir) if !item.zstd => {
                let path = dir.join(sanitize(&item.name));
                let mut f = tokio::fs::File::create(&path).await?;
                let mut limited = (&mut recv).take(item.wire_size);
                let n = tokio::io::copy(&mut limited, &mut f).await?;
                if n != item.wire_size {
                    bail!("stream ended early");
                }
                f.flush().await?;
                paths.push(path);
            }
            _ => {
                let mut buf = vec![0u8; item.wire_size as usize];
                recv.read_exact(&mut buf).await?;
                let data = if item.zstd {
                    tokio::task::spawn_blocking(move || zstd::decode_all(&buf[..])).await??
                } else {
                    buf
                };
                match &dir {
                    Some(dir) => {
                        let path = dir.join(sanitize(&item.name));
                        tokio::fs::write(&path, &data).await?;
                        paths.push(path);
                    }
                    None => mem_items.push(data),
                }
            }
        }
    }
    let _ = send.finish();

    if *st.latest_incoming.lock().expect("lock") != offer.id {
        tracing::debug!("newer clipboard offer arrived; discarding pulled payload");
        return Ok(());
    }

    let content = match offer.kind {
        ClipKind::Text => ClipboardContent::Text {
            text: String::from_utf8_lossy(mem_items.first().map(|v| &v[..]).unwrap_or_default())
                .into_owned(),
            html: None,
        },
        ClipKind::Image => ClipboardContent::Image {
            png: mem_items.into_iter().next().unwrap_or_default(),
        },
        ClipKind::Files => ClipboardContent::Files { paths },
    };
    st.guard.lock().expect("lock").mark_applied(offer.hash);
    // Our canonical hash may differ from the sender's for files written to a
    // new place; record ours too.
    let c2 = content.clone();
    if let Ok(h) = tokio::task::spawn_blocking(move || content_hash(&c2)).await {
        st.guard.lock().expect("lock").mark_applied(h);
    }
    tracing::info!(kind = ?offer.kind, bytes = offer.total_size, "received clipboard payload");
    apply(shared, content).await;
    Ok(())
}

/// Keep only a safe file name (no directories, no traversal).
fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    match cleaned.as_str() {
        "" | "." | ".." => "file".into(),
        _ => cleaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("a\\b.txt"), "b.txt");
        assert_eq!(sanitize(".."), "file");
        assert_eq!(sanitize("ok name.png"), "ok name.png");
    }
}
