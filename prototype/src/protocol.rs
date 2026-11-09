use serde::{Deserialize , Serialize};
use std::time;

pub const PROTOCOL_VERSION: u32 = 1;


#[derive(Clone , Debug , Serialize , Deserialize)]
pub struct Identity {
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub protocol_version: u32,
    pub tcp_port: u16,
    pub capabilities: Vec<String>,
}

#[derive(Clone , Debug , Serialize , Deserialize)]
pub struct Packet {
    pub id: u128,
    #[serde(rename="type")]
    pub kind: String,
    pub body: serde_json::Value,
}

impl Packet{
    pub fn new(kind: &str, body: serde_json::Value) -> Self {
        let id = time::SystemTime::now().duration_since(time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        Packet{id , kind: kind.to_string() , body}
    }
}


pub const TYPE_IDENTITY: &str = "linker.identity";
pub const TYPE_PING: &str = "linker.ping";
pub const TYPE_PONG: &str = "linker.pong";
