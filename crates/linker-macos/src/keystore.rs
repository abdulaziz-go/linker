//! Login Keychain storage (generic password, service "io.linker").

use security_framework::passwords::{get_generic_password, set_generic_password};

const SERVICE: &str = "io.linker";
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

pub fn get(name: &str) -> anyhow::Result<Option<String>> {
    match get_generic_password(SERVICE, name) {
        Ok(bytes) => Ok(Some(String::from_utf8(bytes)?)),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Keychain read failed: {e}")),
    }
}

pub fn set(name: &str, secret: &str) -> anyhow::Result<()> {
    set_generic_password(SERVICE, name, secret.as_bytes())
        .map_err(|e| anyhow::anyhow!("Keychain write failed: {e}"))
}
