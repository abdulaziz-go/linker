//! Secret Service (GNOME Keyring / KWallet) through libsecret's `secret-tool`.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

const SERVICE: &str = "io.linker";

pub fn available() -> bool {
    crate::session::refresh_env();
    crate::dnd::which("secret-tool").is_some()
        && (std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
            || std::env::var_os("XDG_RUNTIME_DIR")
                .is_some_and(|d| std::path::Path::new(&d).join("bus").exists()))
}

pub fn get(name: &str) -> anyhow::Result<Option<String>> {
    crate::session::refresh_env();
    let out = Command::new("secret-tool")
        .args(["lookup", "service", SERVICE, "key", name])
        .stdin(Stdio::null())
        .output()
        .context("running secret-tool")?;
    if out.status.success() && !out.stdout.is_empty() {
        return Ok(Some(String::from_utf8(out.stdout)?));
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.trim().is_empty() {
        // secret-tool exits 1 silently when the item doesn't exist.
        Ok(None)
    } else {
        bail!("secret-tool: {}", err.trim())
    }
}

pub fn set(name: &str, secret: &str) -> anyhow::Result<()> {
    crate::session::refresh_env();
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            "--label=Linker device identity",
            "service",
            SERVICE,
            "key",
            name,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running secret-tool")?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(secret.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "secret-tool store failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Read back: a locked/absent keyring can "succeed" without storing.
    match get(name)? {
        Some(s) if s == secret => Ok(()),
        _ => bail!("secret-tool did not persist the key"),
    }
}
