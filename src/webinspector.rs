use std::borrow::Cow;

use anyhow::{bail, Result};
use idevice::{Idevice, IdeviceService};
use plist::{Dictionary, Value};

pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Client for the `com.apple.webinspector` service.
#[derive(Debug)]
pub struct WebInspectorService {
    dev: Idevice,
}

impl IdeviceService for WebInspectorService {
    fn service_name() -> Cow<'static, str> {
        Cow::Borrowed("com.apple.webinspector")
    }

    async fn from_stream(dev: Idevice) -> Result<Self, idevice::IdeviceError> {
        Ok(Self { dev })
    }
}

impl WebInspectorService {
    pub fn into_socket(self) -> Option<Box<dyn idevice::ReadWrite>> {
        self.dev.get_socket()
    }
}

/// Encode a binary plist with the 4-byte big-endian length prefix used by
/// webinspectord.
pub fn encode_packet(value: &Value) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    value.to_writer_binary(&mut body)?;
    if body.len() > MAX_MESSAGE_SIZE {
        bail!("webinspector message too large ({} bytes)", body.len());
    }
    let mut packet = Vec::with_capacity(body.len() + 4);
    packet.extend_from_slice(&(body.len() as u32).to_be_bytes());
    packet.extend_from_slice(&body);
    Ok(packet)
}

pub fn rpc(selector: &str, args: Dictionary) -> Value {
    let mut message = Dictionary::new();
    message.insert(
        "__selector".to_string(),
        Value::String(selector.to_string()),
    );
    message.insert("__argument".to_string(), Value::Dictionary(args));
    Value::Dictionary(message)
}

pub fn with_connection_id(mut args: Dictionary, connection_id: &str) -> Dictionary {
    args.insert(
        "WIRConnectionIdentifierKey".to_string(),
        Value::String(connection_id.to_string()),
    );
    args
}

pub fn selector(message: &Value) -> Option<&str> {
    message
        .as_dictionary()
        .and_then(|d| d.get("__selector"))
        .and_then(Value::as_string)
}

pub fn argument(message: &Value) -> Option<&Dictionary> {
    message
        .as_dictionary()
        .and_then(|d| d.get("__argument"))
        .and_then(Value::as_dictionary)
}

pub fn get_dict<'a>(dict: &'a Dictionary, key: &str) -> Option<&'a Dictionary> {
    dict.get(key).and_then(Value::as_dictionary)
}

pub fn get_str<'a>(dict: &'a Dictionary, key: &str) -> Option<&'a str> {
    dict.get(key).and_then(Value::as_string)
}

pub fn get_uint(dict: &Dictionary, key: &str) -> Option<u64> {
    dict.get(key).and_then(Value::as_unsigned_integer)
}

pub fn get_data<'a>(dict: &'a Dictionary, key: &str) -> Option<&'a [u8]> {
    dict.get(key).and_then(Value::as_data)
}
