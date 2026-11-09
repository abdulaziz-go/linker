//! GNOME Do Not Disturb = `org.gnome.desktop.notifications show-banners`
//! (DND on ⇔ show-banners false). Uses the `gsettings` CLI so we don't link
//! GLib; changes are observed with `gsettings monitor`.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use linker_platform::{Cap, Unsupported};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

const SCHEMA: &str = "org.gnome.desktop.notifications";
const KEY: &str = "show-banners";

pub fn get() -> anyhow::Result<bool> {
    crate::session::refresh_env();
    let out = Command::new("gsettings")
        .args(["get", SCHEMA, KEY])
        .output()
        .context("running gsettings")?;
    if !out.status.success() {
        bail!(
            "gsettings get failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v = String::from_utf8_lossy(&out.stdout);
    parse(&v).context("unexpected gsettings output")
}

pub fn set(on: bool) -> anyhow::Result<()> {
    crate::session::refresh_env();
    let banners = if on { "false" } else { "true" };
    let st = Command::new("gsettings")
        .args(["set", SCHEMA, KEY, banners])
        .status()
        .context("running gsettings")?;
    if !st.success() {
        bail!("gsettings set failed");
    }
    Ok(())
}

/// `true` if DND is on.
fn parse(s: &str) -> Option<bool> {
    let v = s.trim().rsplit(' ').next()?.trim();
    match v {
        "true" => Some(false),
        "false" => Some(true),
        _ => None,
    }
}

pub fn watch(tx: mpsc::Sender<bool>) -> Cap<()> {
    if which("gsettings").is_none() {
        return Err(Unsupported::new(
            "gsettings not found (not a GNOME system?)",
        ));
    }
    tokio::spawn(async move {
        loop {
            crate::session::refresh_env();
            let child = tokio::process::Command::new("gsettings")
                .args(["monitor", SCHEMA, KEY])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            match child {
                Ok(mut child) => {
                    let out = child.stdout.take().expect("piped");
                    let mut lines = BufReader::new(out).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(on) = parse(&line) {
                            if tx.send(on).await.is_err() {
                                return;
                            }
                        }
                    }
                    let _ = child.wait().await;
                }
                Err(e) => tracing::debug!("gsettings monitor: {e}"),
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    Ok(())
}

pub fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| std::path::Path::new(d).join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_gsettings_output() {
        assert_eq!(super::parse("true\n"), Some(false));
        assert_eq!(super::parse("false"), Some(true));
        assert_eq!(super::parse("show-banners: false"), Some(true));
        assert_eq!(super::parse("garbage"), None);
    }
}
