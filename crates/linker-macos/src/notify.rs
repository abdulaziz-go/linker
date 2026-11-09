//! Notifications on macOS.
//!
//! **Showing** our own banners: `UNUserNotificationCenter` only works from a
//! signed app bundle with a bundle identifier; linkerd is a plain launchd
//! binary, so we use `osascript -e 'display notification …'` (shown under
//! "Script Editor" in System Settings → Notifications).
//!
//! **Capturing** other apps' notifications has no public API. We read the
//! Notification Center database written by `usernoted` (needs Full Disk
//! Access for linkerd). Location and schema are version-dependent:
//!
//! * macOS 15+ : `~/Library/Group Containers/group.com.apple.usernoted/db2/db`
//! * macOS ≤14 : `$(getconf DARWIN_USER_DIR)com.apple.notificationcenter/db2/db`
//!
//! Each `record.data` is a binary plist with `app` and `req.{titl,subt,body}`.
//! This is best effort and may need updating after a macOS release.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context};
use linker_platform::{now_ms, Cap, Notification, Unsupported};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::mpsc;

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn show(n: &Notification) -> anyhow::Result<()> {
    let title = if n.app.is_empty() || n.app == "Linker" {
        n.title.clone()
    } else {
        format!("{} — {}", n.app, n.title)
    };
    let mut script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape(&n.body),
        escape(&title)
    );
    if !n.subtitle.is_empty() {
        script.push_str(&format!(" subtitle \"{}\"", escape(&n.subtitle)));
    }
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .context("running osascript")?;
    Ok(())
}

const CS_DARWIN_USER_DIR: libc::c_int = 65537;

fn darwin_user_dir() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: buffer is valid for its length.
    let n = unsafe {
        libc::confstr(
            CS_DARWIN_USER_DIR,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    Some(PathBuf::from(String::from_utf8(buf).ok()?))
}

pub fn db_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = dirs::home_dir() {
        v.push(h.join("Library/Group Containers/group.com.apple.usernoted/db2/db"));
    }
    if let Some(d) = darwin_user_dir() {
        v.push(d.join("com.apple.notificationcenter/db2/db"));
    }
    v
}

fn open_db() -> anyhow::Result<(Connection, PathBuf)> {
    let mut last_err = None;
    for p in db_candidates() {
        if !p.exists() {
            continue;
        }
        match Connection::open_with_flags(
            &p,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .and_then(|c| {
            c.query_row("SELECT MAX(rec_id) FROM record", [], |_| Ok(()))
                .map(|_| c)
        }) {
            Ok(c) => return Ok((c, p)),
            Err(e) => last_err = Some(format!("{}: {e}", p.display())),
        }
    }
    match last_err {
        Some(e) => bail!(
            "cannot read the Notification Center database ({e}) — grant Full Disk Access to linkerd \
             (System Settings → Privacy & Security → Full Disk Access)"
        ),
        None => bail!("Notification Center database not found for macOS {}", crate::objc::macos_version()),
    }
}

struct Row {
    id: i64,
    app: String,
    data: Vec<u8>,
}

fn parse_record(app_col: &str, data: &[u8]) -> Option<Notification> {
    let v = plist::Value::from_reader(std::io::Cursor::new(data)).ok()?;
    let d = v.as_dictionary()?;
    let req = d.get("req")?.as_dictionary()?;
    let get = |k: &str| {
        req.get(k)
            .and_then(|v| v.as_string())
            .unwrap_or_default()
            .to_string()
    };
    let bundle = d
        .get("app")
        .and_then(|v| v.as_string())
        .unwrap_or(app_col)
        .to_string();
    let app = bundle.rsplit('.').next().unwrap_or(&bundle).to_string();
    let n = Notification {
        app,
        title: get("titl"),
        subtitle: get("subt"),
        body: get("body"),
        timestamp_ms: now_ms(),
    };
    if n.title.is_empty() && n.body.is_empty() {
        return None;
    }
    // Don't bounce Linker's own banners (posted through osascript).
    let own = (bundle.contains("ScriptEditor") || bundle.contains("osascript"))
        && n.title.starts_with("Linker");
    (!own).then_some(n)
}

pub fn watch(tx: mpsc::Sender<Notification>) -> Cap<()> {
    let (conn, path) = open_db().map_err(Unsupported::from)?;
    tracing::info!(db = %path.display(), "watching Notification Center database");
    std::thread::Builder::new()
        .name("linker-notifications".into())
        .spawn(move || {
            let mut conn = conn;
            let mut last: i64 = conn
                .query_row("SELECT COALESCE(MAX(rec_id), 0) FROM record", [], |r| r.get(0))
                .unwrap_or(0);
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if tx.is_closed() {
                    return;
                }
                let rows: rusqlite::Result<Vec<Row>> = (|| {
                    let mut st = conn.prepare_cached(
                        "SELECT r.rec_id, COALESCE(a.identifier, ''), r.data FROM record r \
                         LEFT JOIN app a ON r.app_id = a.app_id WHERE r.rec_id > ?1 ORDER BY r.rec_id",
                    )?;
                    let it = st.query_map([last], |r| Ok(Row { id: r.get(0)?, app: r.get(1)?, data: r.get(2)? }))?;
                    it.collect()
                })();
                match rows {
                    Ok(rows) => {
                        for row in rows {
                            last = last.max(row.id);
                            if let Some(n) = parse_record(&row.app, &row.data) {
                                if tx.blocking_send(n).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("notification db query failed: {e}; reopening");
                        std::thread::sleep(Duration::from_secs(5));
                        if let Ok((c, _)) = open_db() {
                            conn = c;
                        }
                    }
                }
            }
        })
        .map_err(|e| Unsupported::new(e.to_string()))?;
    Ok(())
}
