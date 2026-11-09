//! Local control socket (Unix domain socket, mode 0600) used by the CLI:
//! `linkerd status`, `linkerd pair`, `linkerd unpair`.
//!
//! Protocol: newline-delimited JSON.
//!
//! ```text
//! → {"cmd":"status"}            ← <Status JSON>
//! → {"cmd":"unpair"}            ← {"ok":true}
//! → {"cmd":"pair"}              ← {"event":"waiting"}
//!                               ← {"event":"sas","code":"123 456","peer_name":..,"peer_id":..}
//! → {"cmd":"decide","accept":true}
//!                               ← {"event":"paired","peer_name":..} | {"event":"failed","reason":..}
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::pairing::PairEvent;
use crate::DaemonHandle;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pair,
    Decide { accept: bool },
    Unpair,
}

pub async fn serve(path: PathBuf, handle: DaemonHandle) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        // Stale socket from a previous run? Only remove it if nobody answers.
        if UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("another linkerd is already running ({})", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handle = handle.clone();
            tokio::spawn(async move {
                if let Err(e) = client(stream, handle).await {
                    tracing::debug!("control client: {e:#}");
                }
            });
        }
    });
    Ok(())
}

async fn send_json<T: Serialize>(
    w: &mut (impl AsyncWriteExt + Unpin),
    v: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

async fn client(stream: UnixStream, handle: DaemonHandle) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send_json(&mut w, &serde_json::json!({"error": e.to_string()})).await?;
                continue;
            }
        };
        match req {
            Request::Status => send_json(&mut w, &handle.status()).await?,
            Request::Unpair => {
                handle.unpair()?;
                send_json(&mut w, &serde_json::json!({"ok": true})).await?;
            }
            Request::Decide { accept } => {
                let ok = handle.pair_decide(accept);
                send_json(&mut w, &serde_json::json!({"ok": ok})).await?;
            }
            Request::Pair => {
                let mut events = handle.pair_begin();
                loop {
                    tokio::select! {
                        ev = events.recv() => {
                            let Some(ev) = ev else { break };
                            let done = matches!(ev, PairEvent::Paired { .. } | PairEvent::Failed { .. });
                            send_json(&mut w, &ev).await?;
                            if done { break; }
                        }
                        line = lines.next_line() => {
                            match line? {
                                Some(l) => {
                                    if let Ok(Request::Decide { accept }) = serde_json::from_str(&l) {
                                        handle.pair_decide(accept);
                                    }
                                }
                                None => { handle.pair_cancel(); return Ok(()); }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Minimal client used by the CLI.
pub struct ControlClient {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl ControlClient {
    pub async fn connect(path: &Path) -> anyhow::Result<ControlClient> {
        let s = UnixStream::connect(path).await.with_context(|| {
            format!(
                "cannot reach linkerd at {} — is the daemon running?",
                path.display()
            )
        })?;
        let (r, w) = s.into_split();
        Ok(ControlClient {
            lines: BufReader::new(r).lines(),
            w,
        })
    }

    pub async fn send(&mut self, req: &Request) -> anyhow::Result<()> {
        send_json(&mut self.w, req).await
    }

    pub async fn recv(&mut self) -> anyhow::Result<Option<serde_json::Value>> {
        match self.lines.next_line().await? {
            Some(l) => Ok(Some(serde_json::from_str(&l)?)),
            None => Ok(None),
        }
    }
}
