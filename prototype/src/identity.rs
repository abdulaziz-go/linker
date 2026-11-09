use std::path::PathBuf;
use std::fs::{create_dir_all, read_to_string, write};
use crate::protocol::{Identity, PROTOCOL_VERSION};
use anyhow::Result;

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("linker")
}

pub fn load_or_create(tcp_port: u16, capabilities: Vec<String>) -> Result<Identity> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;

    let id_path = dir.join("device_id");
    let device_id = if id_path.exists() {
        std::fs::read_to_string(&id_path)?.trim().to_string()
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(&id_path, &id)?;
        id
    };

    let device_name = gethostname::gethostname().to_string_lossy().to_string();
    let device_type = if cfg!(target_os = "macos") { "laptop" } else { "desktop" };

    Ok(Identity {
        device_id,
        device_name,
        device_type: device_type.to_string(),
        protocol_version: PROTOCOL_VERSION,
        tcp_port,
        capabilities,
    })
}