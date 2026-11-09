//! Sleep / wake via systemd-logind's `PrepareForSleep(bool)` signal.

use std::time::Duration;

use futures_util::StreamExt;
use linker_platform::PowerEvent;
use tokio::sync::mpsc;
use zbus::message::Type;
use zbus::{MatchRule, MessageStream};

pub fn watch(tx: mpsc::Sender<PowerEvent>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&tx).await {
                tracing::debug!("logind watcher: {e:#}");
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

async fn run(tx: &mpsc::Sender<PowerEvent>) -> anyhow::Result<()> {
    let conn = zbus::Connection::system().await?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface("org.freedesktop.login1.Manager")?
        .member("PrepareForSleep")?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, &conn, None).await?;
    tracing::info!("listening for logind sleep/wake signals");
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let going_to_sleep: bool = msg.body().deserialize()?;
        let ev = if going_to_sleep {
            PowerEvent::Sleeping
        } else {
            PowerEvent::Woke
        };
        if tx.send(ev).await.is_err() {
            return Ok(());
        }
    }
    anyhow::bail!("logind signal stream ended")
}
