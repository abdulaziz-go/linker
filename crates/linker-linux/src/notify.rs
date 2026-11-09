//! Native notification banners via D-Bus `org.freedesktop.Notifications`.

use std::collections::HashMap;

use linker_platform::Notification;
use tokio::sync::Mutex;
use zbus::zvariant::Value;

pub struct Notifier {
    conn: std::sync::Arc<Mutex<Option<zbus::Connection>>>,
}

impl Notifier {
    pub fn new() -> Notifier {
        Notifier {
            conn: Default::default(),
        }
    }

    /// Fire-and-forget (the trait method is sync; D-Bus is async).
    pub fn show(&self, n: Notification) {
        let conn = self.conn.clone();
        let fut = async move {
            if let Err(e) = send(&conn, &n).await {
                tracing::warn!("notification failed: {e:#}");
                *conn.lock().await = None; // reconnect next time
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(fut);
            }
            Err(_) => tracing::warn!("no async runtime for notifications"),
        }
    }
}

async fn send(conn: &Mutex<Option<zbus::Connection>>, n: &Notification) -> anyhow::Result<()> {
    let mut g = conn.lock().await;
    if g.is_none() {
        crate::session::refresh_env();
        *g = Some(zbus::Connection::session().await?);
    }
    let c = g.as_ref().expect("set");
    let summary = if n.app.is_empty() || n.app == "Linker" {
        n.title.clone()
    } else if n.title.is_empty() {
        n.app.clone()
    } else {
        format!("{} — {}", n.app, n.title)
    };
    let body = if n.subtitle.is_empty() {
        n.body.clone()
    } else {
        format!("{}\n{}", n.subtitle, n.body)
    };
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("desktop-entry", Value::from("linker"));
    let actions: Vec<&str> = Vec::new();
    c.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "Linker",
            0u32,
            "phone",
            summary.as_str(),
            body.as_str(),
            actions,
            hints,
            -1i32,
        ),
    )
    .await?;
    Ok(())
}
