//! Focus / Do Not Disturb on macOS (no public API exists).
//!
//! * **Read**: `~/Library/DoNotDisturb/DB/Assertions.json` (macOS 12+) lists
//!   active manual Focus assertions; non-empty ⇒ DND on. Needs Full Disk
//!   Access. Scheduled/automatic Focus modes are not reflected there.
//! * **Write**: run the user-created Shortcuts "Linker DND On" /
//!   "Linker DND Off" (Set Focus action) via the `shortcuts` CLI.
//!
//! If reading isn't possible, DND degrades to "set-only": a DND change on
//! Ubuntu still propagates to the Mac, and Mac-side changes are manual.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context};
use linker_platform::{Cap, Unsupported};
use tokio::sync::mpsc;

pub const SHORTCUT_ON: &str = "Linker DND On";
pub const SHORTCUT_OFF: &str = "Linker DND Off";

fn assertions_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/DoNotDisturb/DB/Assertions.json"))
}

pub fn parse_assertions(json: &str) -> anyhow::Result<bool> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .context("unexpected Assertions.json format")?;
    Ok(data.iter().any(|entry| {
        entry
            .get("storeAssertionRecords")
            .and_then(|r| r.as_array())
            .is_some_and(|r| !r.is_empty())
    }))
}

pub fn get() -> anyhow::Result<bool> {
    if crate::objc::macos_major() < 12 {
        bail!("Focus detection needs macOS 12 or newer");
    }
    let p = assertions_path().context("no home directory")?;
    let s = std::fs::read_to_string(&p).with_context(|| {
        format!(
            "cannot read {} — grant Full Disk Access to linkerd",
            p.display()
        )
    })?;
    parse_assertions(&s)
}

pub fn set(on: bool) -> anyhow::Result<()> {
    let name = if on { SHORTCUT_ON } else { SHORTCUT_OFF };
    let out = Command::new("shortcuts")
        .args(["run", name])
        .output()
        .context("running `shortcuts`")?;
    if !out.status.success() {
        bail!(
            "shortcut \"{name}\" failed ({}) — create it in the Shortcuts app (see runbook)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn watch(tx: mpsc::Sender<bool>) -> Cap<()> {
    let initial = get().map_err(Unsupported::from)?;
    std::thread::Builder::new()
        .name("linker-dnd".into())
        .spawn(move || {
            let mut last = initial;
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                if tx.is_closed() {
                    return;
                }
                if let Ok(now) = get() {
                    if now != last {
                        last = now;
                        if tx.blocking_send(now).is_err() {
                            return;
                        }
                    }
                }
            }
        })
        .map_err(|e| Unsupported::new(e.to_string()))?;
    Ok(())
}
