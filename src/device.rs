use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::services::lockdown::LockdownClient;
use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdConnection};
use idevice::{IdeviceService, ReadWrite};
use plist::{Dictionary, Value};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::webinspector::{
    encode_packet, get_data, get_dict, get_str, get_uint, rpc, with_connection_id,
    WebInspectorService, MAX_MESSAGE_SIZE,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub page_num: u32,
    pub page_id: u32,
    pub app_id: String,
    pub app_name: String,
    pub bundle_id: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub os_version: String,
    pub connected: bool,
    pub status: String,
    pub pages: Vec<PageInfo>,
}

#[derive(Debug, Clone)]
struct DeviceMeta {
    name: String,
    os_version: String,
    os_version_tuple: (u32, u32, u32),
    connected: bool,
    status: String,
}

#[derive(Debug, Clone)]
pub struct WsClient {
    pub tx: mpsc::Sender<String>,
    pub page_num: u32,
}

#[derive(Debug)]
pub(crate) enum DeviceCommand {
    Setup {
        sender_id: String,
        page_num: u32,
    },
    Data {
        sender_id: String,
        page_num: u32,
        data: Vec<u8>,
    },
    Close {
        sender_id: String,
        page_num: u32,
    },
    Refresh,
}

#[derive(Debug)]
pub struct DeviceHandle {
    pub udid: String,
    pub device_id: u32,
    pub(crate) cmd_tx: mpsc::Sender<DeviceCommand>,
    meta: RwLock<DeviceMeta>,
    pages: RwLock<Vec<PageInfo>>,
    ws_clients: Mutex<HashMap<String, WsClient>>,
    cancel: CancellationToken,
}

impl DeviceHandle {
    fn new(
        udid: String,
        device_id: u32,
        cancel: CancellationToken,
    ) -> (Self, mpsc::Receiver<DeviceCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(128);
        let handle = Self {
            udid: udid.clone(),
            device_id,
            cmd_tx,
            meta: RwLock::new(DeviceMeta {
                name: udid,
                os_version: "unknown".to_string(),
                os_version_tuple: (0, 0, 0),
                connected: false,
                status: "connecting".to_string(),
            }),
            pages: RwLock::new(Vec::new()),
            ws_clients: Mutex::new(HashMap::new()),
            cancel,
        };
        (handle, cmd_rx)
    }

    pub async fn info(&self) -> DeviceInfo {
        let meta = self.meta.read().await.clone();
        let pages = self.pages.read().await.clone();
        DeviceInfo {
            udid: self.udid.clone(),
            name: meta.name,
            os_version: meta.os_version,
            connected: meta.connected,
            status: meta.status,
            pages,
        }
    }

    pub async fn is_connected(&self) -> bool {
        self.meta.read().await.connected
    }

    pub async fn os_version_tuple(&self) -> (u32, u32, u32) {
        self.meta.read().await.os_version_tuple
    }

    pub async fn pages(&self) -> Vec<PageInfo> {
        self.pages.read().await.clone()
    }

    pub async fn add_ws_client(&self, sender_id: String, client: WsClient) {
        self.ws_clients.lock().await.insert(sender_id, client);
    }

    pub async fn queue_setup(&self, sender_id: &str, page_num: u32) -> bool {
        self.cmd_tx
            .send(DeviceCommand::Setup {
                sender_id: sender_id.to_string(),
                page_num,
            })
            .await
            .is_ok()
    }

    pub async fn queue_data(&self, sender_id: &str, page_num: u32, data: Vec<u8>) -> bool {
        self.cmd_tx
            .send(DeviceCommand::Data {
                sender_id: sender_id.to_string(),
                page_num,
                data,
            })
            .await
            .is_ok()
    }

    pub async fn queue_close(&self, sender_id: &str, page_num: u32) -> bool {
        self.cmd_tx
            .send(DeviceCommand::Close {
                sender_id: sender_id.to_string(),
                page_num,
            })
            .await
            .is_ok()
    }

    pub async fn remove_ws_client(&self, sender_id: &str) {
        self.ws_clients.lock().await.remove(sender_id);
    }

    pub async fn close_clients_for_page(&self, page_num: u32) {
        self.ws_clients
            .lock()
            .await
            .retain(|_, client| client.page_num != page_num);
    }

    pub async fn close_all_clients(&self) {
        self.ws_clients.lock().await.clear();
    }

    async fn set_meta_connected(&self, connected: bool, status: impl Into<String>) {
        let mut meta = self.meta.write().await;
        meta.connected = connected;
        meta.status = status.into();
    }

    async fn set_status(&self, status: impl Into<String>) {
        self.meta.write().await.status = status.into();
    }

    async fn set_device_info(&self, name: String, os_version: String, tuple: (u32, u32, u32)) {
        let mut meta = self.meta.write().await;
        meta.name = name;
        meta.os_version = os_version;
        meta.os_version_tuple = tuple;
    }

    async fn publish_pages(&self, pages: Vec<PageInfo>) {
        *self.pages.write().await = pages;
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbmuxdStatus {
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct DeviceManager {
    devices: RwLock<HashMap<String, Arc<DeviceHandle>>>,
    usbmuxd: RwLock<UsbmuxdStatus>,
    filter: Option<String>,
}

impl DeviceManager {
    pub fn new(filter: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            devices: RwLock::new(HashMap::new()),
            usbmuxd: RwLock::new(UsbmuxdStatus {
                available: true,
                error: None,
            }),
            filter,
        })
    }

    pub async fn get(&self, udid: &str) -> Option<Arc<DeviceHandle>> {
        self.devices.read().await.get(udid).cloned()
    }

    pub async fn usbmuxd_status(&self) -> UsbmuxdStatus {
        self.usbmuxd.read().await.clone()
    }

    pub async fn snapshot(&self) -> Vec<DeviceInfo> {
        let handles: Vec<_> = self.devices.read().await.values().cloned().collect();
        let mut result = Vec::with_capacity(handles.len());
        for handle in handles {
            result.push(handle.info().await);
        }
        result.sort_by(|a, b| a.udid.cmp(&b.udid));
        result
    }

    pub async fn refresh_all(&self) {
        let handles: Vec<_> = self.devices.read().await.values().cloned().collect();
        for handle in handles {
            let _ = handle.cmd_tx.send(DeviceCommand::Refresh).await;
        }
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        info!("Waiting for iOS devices over usbmuxd");
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }

            match UsbmuxdConnection::default().await {
                Ok(mut conn) => match conn.get_devices().await {
                    Ok(devices) => {
                        self.set_usbmuxd_available(true, None).await;
                        self.reconcile(devices, &cancel).await;
                    }
                    Err(e) => {
                        let message = format!("{:#}", anyhow::Error::new(e));
                        self.set_usbmuxd_available(false, Some(message)).await;
                        self.remove_all("usbmuxd unavailable").await;
                    }
                },
                Err(e) => {
                    let message = format!("{:#}", anyhow::Error::new(e));
                    self.set_usbmuxd_available(false, Some(message)).await;
                    self.remove_all("usbmuxd unavailable").await;
                }
            }
        }

        self.remove_all("shutting down").await;
    }

    async fn set_usbmuxd_available(&self, available: bool, error: Option<String>) {
        let changed = {
            let mut state = self.usbmuxd.write().await;
            let changed = state.available != available;
            state.available = available;
            state.error = error.clone();
            changed
        };

        if changed {
            if available {
                info!("usbmuxd is available again");
            } else {
                warn!(
                    "usbmuxd is unavailable; waiting for it to come back: {}",
                    error.as_deref().unwrap_or("unknown error")
                );
            }
        }
    }

    async fn remove_all(&self, reason: &str) {
        let handles: Vec<_> = self
            .devices
            .write()
            .await
            .drain()
            .map(|(_, value)| value)
            .collect();
        for handle in handles {
            info!("Device disconnected: {} ({reason})", handle.udid);
            handle.cancel();
            handle.close_all_clients().await;
        }
    }

    async fn reconcile(
        &self,
        devices: Vec<idevice::usbmuxd::UsbmuxdDevice>,
        cancel: &CancellationToken,
    ) {
        let mut present = HashSet::new();
        for device in devices {
            if let Some(filter) = &self.filter {
                if &device.udid != filter {
                    continue;
                }
            }
            present.insert(device.udid.clone());

            let existing = self.devices.read().await.get(&device.udid).cloned();
            if let Some(existing) = existing {
                if existing.device_id == device.device_id {
                    continue;
                }
                info!(
                    "Device reconnected with a new usbmuxd id: {} ({} -> {})",
                    device.udid, existing.device_id, device.device_id
                );
                self.devices.write().await.remove(&device.udid);
                existing.cancel();
                existing.close_all_clients().await;
            }

            let provider = device.to_provider(
                UsbmuxdAddr::from_env_var().unwrap_or_default(),
                "safari-debug",
            );
            let child_cancel = cancel.child_token();
            let (handle, cmd_rx) =
                DeviceHandle::new(device.udid.clone(), device.device_id, child_cancel.clone());
            let handle = Arc::new(handle);

            info!("Device connected: {} ({})", device.udid, device.udid);
            self.devices
                .write()
                .await
                .insert(device.udid.clone(), handle.clone());
            tokio::spawn(run_device(handle, provider, cmd_rx, child_cancel));
        }

        let removed: Vec<String> = {
            let map = self.devices.read().await;
            map.keys()
                .filter(|udid| !present.contains(*udid))
                .cloned()
                .collect()
        };
        if removed.is_empty() {
            return;
        }

        let mut map = self.devices.write().await;
        for udid in removed {
            if let Some(handle) = map.remove(&udid) {
                info!("Device disconnected: {udid}");
                handle.cancel();
                handle.close_all_clients().await;
            }
        }
    }
}

async fn run_device(
    device: Arc<DeviceHandle>,
    provider: UsbmuxdProvider,
    mut cmd_rx: mpsc::Receiver<DeviceCommand>,
    cancel: CancellationToken,
) {
    let mut attempt = 0u32;
    let mut last_error: Option<String> = None;
    loop {
        if cancel.is_cancelled() {
            break;
        }

        device
            .set_meta_connected(false, "connecting to webinspector")
            .await;

        let started = Instant::now();
        let result = run_session(&device, &provider, &mut cmd_rx, &cancel).await;
        let lasted = started.elapsed();
        if lasted > Duration::from_secs(5) {
            // This was a real session, not a tight retry loop. Reset the
            // backoff and make the next failure worth a warning again.
            attempt = 0;
            last_error = None;
        }

        match result {
            Ok(()) => debug!("webinspector session ended for {}", device.udid),
            Err(e) => {
                let message = format!("{e:#}");
                if is_transport_error(&message) {
                    debug!(
                        "webinspector transport for {} closed: {message}",
                        device.udid
                    );
                    device
                        .set_meta_connected(false, "disconnected; waiting for device")
                        .await;
                } else {
                    let is_new = last_error.as_deref() != Some(message.as_str());
                    if is_new {
                        warn!("webinspector session for {} failed: {message}", device.udid);
                        last_error = Some(message.clone());
                    } else {
                        debug!(
                            "webinspector session for {} still failing: {message}",
                            device.udid
                        );
                    }
                    device
                        .set_meta_connected(false, format!("error: {message}"))
                        .await;
                }
            }
        }

        device.close_all_clients().await;
        device.publish_pages(Vec::new()).await;

        attempt = attempt.saturating_add(1);
        let delay = Duration::from_millis(500 * u64::from(attempt.min(10)));
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn run_session(
    device: &Arc<DeviceHandle>,
    provider: &UsbmuxdProvider,
    cmd_rx: &mut mpsc::Receiver<DeviceCommand>,
    cancel: &CancellationToken,
) -> Result<()> {
    // Drop commands queued while we were disconnected.
    while cmd_rx.try_recv().is_ok() {}

    let session_cancel = CancellationToken::new();
    let _session_guard = CancelOnDrop(session_cancel.clone());

    match tokio::time::timeout(
        Duration::from_secs(10),
        update_device_meta(device, provider),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!("could not read device metadata: {e:#}"),
        Err(_) => debug!("timed out reading device metadata"),
    }

    let service = tokio::time::timeout(
        Duration::from_secs(15),
        WebInspectorService::connect(provider),
    )
    .await
    .map_err(|_| anyhow!("timed out starting com.apple.webinspector"))?
    .context("failed to start com.apple.webinspector (is Web Inspector enabled in Settings > Safari > Advanced?)")?;
    let socket: Box<dyn ReadWrite> = service
        .into_socket()
        .context("webinspector service did not provide a socket")?;

    let (mut read_half, mut write_half) = tokio::io::split(socket);
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Value, String>>(64);

    let reader_cancel = session_cancel.clone();
    tokio::spawn(async move {
        loop {
            let mut header = [0u8; 4];
            let read_header = tokio::select! {
                _ = reader_cancel.cancelled() => return,
                result = read_half.read_exact(&mut header) => result,
            };
            if let Err(e) = read_header {
                let _ = in_tx.send(Err(format!("read header: {e}"))).await;
                return;
            }

            let length = u32::from_be_bytes(header) as usize;
            if length > MAX_MESSAGE_SIZE {
                let _ = in_tx
                    .send(Err(format!("webinspector packet too large: {length}")))
                    .await;
                return;
            }

            let mut body = vec![0u8; length];
            let read_body = tokio::select! {
                _ = reader_cancel.cancelled() => return,
                result = read_half.read_exact(&mut body) => result,
            };
            if let Err(e) = read_body {
                let _ = in_tx.send(Err(format!("read body: {e}"))).await;
                return;
            }

            let value = plist::from_bytes::<Value>(&body).map_err(|e| format!("parse plist: {e}"));
            if in_tx.send(value).await.is_err() {
                return;
            }
        }
    });

    let writer_cancel = session_cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => return,
                packet = write_rx.recv() => match packet {
                    Some(packet) => {
                        if let Err(e) = write_half.write_all(&packet).await {
                            debug!("webinspector write failed: {e}");
                            return;
                        }
                        if let Err(e) = write_half.flush().await {
                            debug!("webinspector flush failed: {e}");
                            return;
                        }
                    }
                    None => return,
                }
            }
        }
    });

    let connection_id = Uuid::new_v4().to_string().to_uppercase();
    let mut session = Session {
        device: device.clone(),
        write_tx: write_tx.clone(),
        connection_id,
        pages: HashMap::new(),
        page_keys: HashMap::new(),
        next_page_num: 1,
        apps: HashMap::new(),
    };

    device
        .set_meta_connected(true, "connected; waiting for Safari pages")
        .await;
    session
        .send_rpc("_rpc_reportIdentifier:", Dictionary::new())
        .await?;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            command = cmd_rx.recv() => {
                let Some(command) = command else { return Ok(()) };
                match command {
                    DeviceCommand::Setup { sender_id, page_num } => {
                        session.forward_socket_setup(&sender_id, page_num).await?;
                    }
                    DeviceCommand::Data { sender_id, page_num, data } => {
                        session.forward_socket_data(&sender_id, page_num, data).await?;
                    }
                    DeviceCommand::Close { sender_id, page_num } => {
                        session.forward_did_close(&sender_id, page_num).await?;
                    }
                    DeviceCommand::Refresh => {
                        session.refresh().await?;
                    }
                }
            }
            incoming = in_rx.recv() => {
                let Some(incoming) = incoming else { return Ok(()) };
                let value = incoming.map_err(anyhow::Error::msg)?;
                session.handle(value).await?;
            }
        }
    }
}

async fn update_device_meta(device: &Arc<DeviceHandle>, provider: &UsbmuxdProvider) -> Result<()> {
    let mut lockdown = LockdownClient::connect(provider)
        .await
        .context("failed to connect to lockdownd")?;
    let pairing = provider
        .get_pairing_file()
        .await
        .context("failed to read pairing record from usbmuxd")?;
    lockdown
        .start_session(&pairing)
        .await
        .context("lockdown StartSession failed")?;

    let name = lockdown
        .get_value(Some("DeviceName"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(str::to_string));
    let version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(str::to_string));

    let name = name.unwrap_or_else(|| device.udid.clone());
    let version = version.unwrap_or_else(|| "unknown".to_string());
    let tuple = parse_version(&version);
    device.set_device_info(name, version, tuple).await;
    Ok(())
}

fn parse_version(version: &str) -> (u32, u32, u32) {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

/// Errors that normally mean the cable was unplugged or usbmuxd stopped.
/// These are expected and should not produce a warning for every retry.
fn is_transport_error(message: &str) -> bool {
    [
        "device disconnected",
        "peer closed",
        "close_notify",
        "Connection refused",
        "Connection reset",
        "Broken pipe",
        "device socket io failed",
        "os error 111",
        "os error 104",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

async fn send_raw_message<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    connection_id: &str,
    selector: &str,
    args: Dictionary,
) -> Result<()> {
    let packet = encode_packet(&rpc(selector, with_connection_id(args, connection_id)))?;
    write.write_all(&packet).await?;
    write.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_errors_are_recognized() {
        assert!(is_transport_error(
            "read header: peer closed connection without sending TLS close_notify"
        ));
        assert!(is_transport_error(
            "device socket io failed: Connection refused (os error 111)"
        ));
        assert!(!is_transport_error(
            "failed to start com.apple.webinspector: service error"
        ));
    }

    #[test]
    fn os_versions_are_parsed() {
        assert_eq!(parse_version("27.2"), (27, 2, 0));
        assert_eq!(parse_version("18.4.1"), (18, 4, 1));
        assert_eq!(parse_version("unknown"), (0, 0, 0));
    }
}

#[derive(Debug, Clone)]
struct AppInfo {
    bundle_id: String,
    name: String,
}

struct Session {
    device: Arc<DeviceHandle>,
    write_tx: mpsc::Sender<Vec<u8>>,
    connection_id: String,
    pages: HashMap<u32, PageInfo>,
    page_keys: HashMap<(String, u32), u32>,
    next_page_num: u32,
    apps: HashMap<String, AppInfo>,
}

impl Session {
    async fn send_rpc(&self, selector: &str, args: Dictionary) -> Result<()> {
        let message = rpc(selector, with_connection_id(args, &self.connection_id));
        let packet = encode_packet(&message)?;
        self.write_tx
            .send(packet)
            .await
            .map_err(|_| anyhow!("webinspector writer stopped"))?;
        Ok(())
    }

    async fn send_listing(&self, app_id: &str) -> Result<()> {
        let mut args = Dictionary::new();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            Value::String(app_id.to_string()),
        );
        self.send_rpc("_rpc_forwardGetListing:", args).await
    }

    async fn handle(&mut self, message: Value) -> Result<()> {
        let Some(selector) = crate::webinspector::selector(&message) else {
            debug!("webinspector message without __selector");
            return Ok(());
        };
        let args = crate::webinspector::argument(&message);

        match selector {
            "_rpc_reportSetup:" => {
                self.device
                    .set_meta_connected(true, "connected; waiting for Safari pages")
                    .await;
            }
            "_rpc_reportConnectedApplicationList:" => {
                self.device
                    .set_meta_connected(true, "connected; waiting for Safari pages")
                    .await;
                if let Some(args) = args {
                    self.handle_connected_apps(args).await?;
                }
                self.publish().await;
            }
            "_rpc_applicationConnected:" | "_rpc_applicationUpdated:" => {
                if let Some(args) = args {
                    self.handle_application(args).await?;
                }
                self.publish().await;
            }
            "_rpc_applicationDisconnected:" => {
                if let Some(app_id) = args.and_then(|a| get_str(a, "WIRApplicationIdentifierKey")) {
                    self.remove_app(app_id).await;
                    self.publish().await;
                }
            }
            "_rpc_applicationSentListing:" => {
                if let Some(args) = args {
                    self.handle_listing(args).await?;
                }
                self.publish().await;
            }
            "_rpc_applicationSentData:" => {
                if let Some(args) = args {
                    self.handle_sent_data(args).await;
                }
            }
            "_rpc_reportCurrentState:" | "_rpc_reportConnectedDriverList:" => {}
            other => debug!("ignoring webinspector selector {other}"),
        }
        Ok(())
    }

    async fn handle_connected_apps(&mut self, args: &Dictionary) -> Result<()> {
        let Some(dict) = get_dict(args, "WIRApplicationDictionaryKey") else {
            return Ok(());
        };

        let mut current = HashSet::new();
        for (key, value) in dict {
            let Some(app) = value.as_dictionary() else {
                continue;
            };
            let app_id = get_str(app, "WIRApplicationIdentifierKey")
                .unwrap_or(key)
                .to_string();
            current.insert(app_id.clone());
            self.upsert_app(&app_id, app);
            self.send_listing(&app_id).await?;
        }

        let stale: Vec<String> = self
            .apps
            .keys()
            .filter(|id| !current.contains(*id))
            .cloned()
            .collect();
        for app_id in stale {
            self.remove_app(&app_id).await;
        }
        Ok(())
    }

    async fn handle_application(&mut self, args: &Dictionary) -> Result<()> {
        let Some(app_id) = get_str(args, "WIRApplicationIdentifierKey") else {
            return Ok(());
        };
        let app_id = app_id.to_string();
        self.upsert_app(&app_id, args);
        self.send_listing(&app_id).await
    }

    fn upsert_app(&mut self, app_id: &str, app: &Dictionary) {
        let bundle_id = get_str(app, "WIRApplicationBundleIdentifierKey")
            .unwrap_or("")
            .to_string();
        let name = get_str(app, "WIRApplicationNameKey")
            .filter(|name| !name.is_empty())
            .unwrap_or(app_id)
            .to_string();
        self.apps
            .insert(app_id.to_string(), AppInfo { bundle_id, name });
    }

    async fn handle_listing(&mut self, args: &Dictionary) -> Result<()> {
        let Some(app_id) = get_str(args, "WIRApplicationIdentifierKey") else {
            return Ok(());
        };
        let app_id = app_id.to_string();
        let Some(listing) = get_dict(args, "WIRListingKey") else {
            return Ok(());
        };

        let mut seen = HashSet::new();
        for (key, value) in listing {
            let Some(page) = value.as_dictionary() else {
                continue;
            };
            let page_id = get_uint(page, "WIRPageIdentifierKey")
                .map(|v| v as u32)
                .or_else(|| key.parse().ok())
                .unwrap_or(0);
            seen.insert(page_id);

            let page_num = match self.page_keys.get(&(app_id.clone(), page_id)) {
                Some(n) => *n,
                None => {
                    let n = self.next_page_num;
                    self.next_page_num = self.next_page_num.saturating_add(1);
                    self.page_keys.insert((app_id.clone(), page_id), n);
                    n
                }
            };

            let app = self.apps.get(&app_id);
            let info = PageInfo {
                page_num,
                page_id,
                app_id: app_id.clone(),
                app_name: app
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| app_id.clone()),
                bundle_id: app.map(|a| a.bundle_id.clone()).unwrap_or_default(),
                title: get_str(page, "WIRTitleKey").unwrap_or("").to_string(),
                url: get_str(page, "WIRURLKey").unwrap_or("").to_string(),
            };

            if let Some(existing) = self.pages.get(&page_num) {
                if existing.title != info.title || existing.url != info.url {
                    debug!(
                        "page {} changed: {} -> {}",
                        info.page_num, existing.url, info.url
                    );
                }
            }
            self.pages.insert(page_num, info);
        }

        let stale: Vec<u32> = self
            .pages
            .values()
            .filter(|page| page.app_id == app_id && !seen.contains(&page.page_id))
            .map(|page| page.page_num)
            .collect();
        for page_num in stale {
            self.remove_page(page_num).await;
        }
        Ok(())
    }

    async fn handle_sent_data(&self, args: &Dictionary) {
        let Some(destination) = get_str(args, "WIRDestinationKey") else {
            return;
        };
        let Some(data) = get_data(args, "WIRMessageDataKey") else {
            return;
        };
        let Ok(text) = std::str::from_utf8(data) else {
            warn!("webinspector sent non-UTF-8 inspector data");
            return;
        };

        let clients = self.device.ws_clients.lock().await;
        if let Some(client) = clients.get(destination) {
            if let Err(e) = client.tx.try_send(text.to_string()) {
                debug!("could not deliver inspector message to browser: {e}");
            }
        }
    }

    async fn forward_socket_setup(&self, sender_id: &str, page_num: u32) -> Result<()> {
        let Some(page) = self.pages.get(&page_num) else {
            debug!("setup requested for unknown page {page_num}");
            return Ok(());
        };
        let mut args = Dictionary::new();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            Value::String(page.app_id.clone()),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            Value::Integer((page.page_id as u64).into()),
        );
        args.insert(
            "WIRSenderKey".to_string(),
            Value::String(sender_id.to_string()),
        );
        args.insert("WIRAutomaticallyPause".to_string(), Value::Boolean(false));
        self.send_rpc("_rpc_forwardSocketSetup:", args).await
    }

    async fn forward_socket_data(
        &self,
        sender_id: &str,
        page_num: u32,
        data: Vec<u8>,
    ) -> Result<()> {
        let Some(page) = self.pages.get(&page_num) else {
            debug!("data requested for unknown page {page_num}");
            return Ok(());
        };
        let mut args = Dictionary::new();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            Value::String(page.app_id.clone()),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            Value::Integer((page.page_id as u64).into()),
        );
        args.insert(
            "WIRSenderKey".to_string(),
            Value::String(sender_id.to_string()),
        );
        args.insert("WIRSocketDataKey".to_string(), Value::Data(data));
        self.send_rpc("_rpc_forwardSocketData:", args).await
    }

    async fn forward_did_close(&self, sender_id: &str, page_num: u32) -> Result<()> {
        let Some(page) = self.pages.get(&page_num) else {
            return Ok(());
        };
        let mut args = Dictionary::new();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            Value::String(page.app_id.clone()),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            Value::Integer((page.page_id as u64).into()),
        );
        args.insert(
            "WIRSenderKey".to_string(),
            Value::String(sender_id.to_string()),
        );
        self.send_rpc("_rpc_forwardDidClose:", args).await
    }

    async fn refresh(&self) -> Result<()> {
        self.send_rpc("_rpc_getConnectedApplications:", Dictionary::new())
            .await?;
        for app_id in self.apps.keys() {
            self.send_listing(app_id).await?;
        }
        Ok(())
    }

    async fn remove_page(&mut self, page_num: u32) {
        self.pages.remove(&page_num);
        self.page_keys.retain(|_, n| *n != page_num);
        self.device.close_clients_for_page(page_num).await;
    }

    async fn remove_app(&mut self, app_id: &str) {
        self.apps.remove(app_id);
        let pages: Vec<u32> = self
            .pages
            .values()
            .filter(|page| page.app_id == app_id)
            .map(|page| page.page_num)
            .collect();
        for page_num in pages {
            self.remove_page(page_num).await;
        }
    }

    async fn publish(&self) {
        let mut pages: Vec<PageInfo> = self.pages.values().cloned().collect();
        pages.sort_by_key(|page| page.page_num);
        let status = if pages.is_empty() {
            "connected; waiting for Safari pages".to_string()
        } else {
            format!(
                "connected; {} debuggable page{}",
                pages.len(),
                if pages.len() == 1 { "" } else { "s" }
            )
        };
        self.device.publish_pages(pages).await;
        self.device.set_status(status).await;
    }
}

/// One-shot device snapshot for `safari-debug devices`.
pub async fn snapshot_devices() -> Result<Vec<DeviceInfo>> {
    let mut conn = UsbmuxdConnection::default()
        .await
        .context("failed to connect to usbmuxd")?;
    let devices = conn.get_devices().await.context("failed to list devices")?;

    let mut result = Vec::new();
    for device in devices {
        let provider = device.to_provider(
            UsbmuxdAddr::from_env_var().unwrap_or_default(),
            "safari-debug",
        );
        let (device_info, pages) = match collect_pages(&provider).await {
            Ok(value) => value,
            Err(e) => (
                DeviceInfo {
                    udid: device.udid.clone(),
                    name: device.udid.clone(),
                    os_version: "unknown".into(),
                    connected: false,
                    status: format!("error: {e:#}"),
                    pages: Vec::new(),
                },
                Vec::new(),
            ),
        };
        result.push(DeviceInfo {
            pages,
            ..device_info
        });
    }
    Ok(result)
}

/// Connect briefly to one device and collect its page listing.
async fn collect_pages(provider: &UsbmuxdProvider) -> Result<(DeviceInfo, Vec<PageInfo>)> {
    let device = Arc::new(
        DeviceHandle::new(
            provider.udid.clone(),
            provider.device_id,
            CancellationToken::new(),
        )
        .0,
    );
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        update_device_meta(&device, provider),
    )
    .await;

    let service = tokio::time::timeout(
        Duration::from_secs(15),
        WebInspectorService::connect(provider),
    )
    .await
    .map_err(|_| anyhow!("timed out starting com.apple.webinspector"))??;
    let socket = service.into_socket().context("no webinspector socket")?;
    let (mut read_half, mut write_half) = tokio::io::split(socket);
    let connection_id = Uuid::new_v4().to_string().to_uppercase();

    send_raw_message(
        &mut write_half,
        &connection_id,
        "_rpc_reportIdentifier:",
        Dictionary::new(),
    )
    .await?;

    let mut apps: HashMap<String, AppInfo> = HashMap::new();
    let mut pages: HashMap<u32, PageInfo> = HashMap::new();
    let mut page_keys: HashMap<(String, u32), u32> = HashMap::new();
    let mut next_page_num = 1u32;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let mut header = [0u8; 4];
        let read =
            tokio::time::timeout(Duration::from_secs(3), read_half.read_exact(&mut header)).await;
        let Ok(Ok(_)) = read else {
            break;
        };
        let length = u32::from_be_bytes(header) as usize;
        if length > MAX_MESSAGE_SIZE {
            break;
        }
        let mut body = vec![0u8; length];
        if tokio::time::timeout(Duration::from_secs(3), read_half.read_exact(&mut body))
            .await
            .ok()
            .and_then(|r| r.ok())
            .is_none()
        {
            break;
        }
        let value: Value = match plist::from_bytes(&body) {
            Ok(v) => v,
            Err(_) => break,
        };
        let Some(selector) = crate::webinspector::selector(&value) else {
            continue;
        };
        let args = crate::webinspector::argument(&value);
        match selector {
            "_rpc_reportConnectedApplicationList:" => {
                if let Some(args) = args {
                    if let Some(dict) = get_dict(args, "WIRApplicationDictionaryKey") {
                        for (key, value) in dict {
                            let Some(app) = value.as_dictionary() else {
                                continue;
                            };
                            let app_id = get_str(app, "WIRApplicationIdentifierKey")
                                .unwrap_or(key)
                                .to_string();
                            apps.insert(
                                app_id.clone(),
                                AppInfo {
                                    bundle_id: get_str(app, "WIRApplicationBundleIdentifierKey")
                                        .unwrap_or("")
                                        .to_string(),
                                    name: get_str(app, "WIRApplicationNameKey")
                                        .unwrap_or(app_id.as_str())
                                        .to_string(),
                                },
                            );
                            let mut listing_args = Dictionary::new();
                            listing_args.insert(
                                "WIRApplicationIdentifierKey".to_string(),
                                Value::String(app_id),
                            );
                            send_raw_message(
                                &mut write_half,
                                &connection_id,
                                "_rpc_forwardGetListing:",
                                listing_args,
                            )
                            .await?;
                        }
                    }
                }
            }
            "_rpc_applicationConnected:" | "_rpc_applicationUpdated:" => {
                if let Some(args) = args {
                    if let Some(app_id) = get_str(args, "WIRApplicationIdentifierKey") {
                        apps.insert(
                            app_id.to_string(),
                            AppInfo {
                                bundle_id: get_str(args, "WIRApplicationBundleIdentifierKey")
                                    .unwrap_or("")
                                    .to_string(),
                                name: get_str(args, "WIRApplicationNameKey")
                                    .unwrap_or(app_id)
                                    .to_string(),
                            },
                        );
                        let mut listing_args = Dictionary::new();
                        listing_args.insert(
                            "WIRApplicationIdentifierKey".to_string(),
                            Value::String(app_id.to_string()),
                        );
                        send_raw_message(
                            &mut write_half,
                            &connection_id,
                            "_rpc_forwardGetListing:",
                            listing_args,
                        )
                        .await?;
                    }
                }
            }
            "_rpc_applicationSentListing:" => {
                if let Some(args) = args {
                    let app_id = get_str(args, "WIRApplicationIdentifierKey")
                        .unwrap_or("")
                        .to_string();
                    if let Some(listing) = get_dict(args, "WIRListingKey") {
                        for (key, value) in listing {
                            let Some(page) = value.as_dictionary() else {
                                continue;
                            };
                            let page_id = get_uint(page, "WIRPageIdentifierKey")
                                .map(|v| v as u32)
                                .or_else(|| key.parse().ok())
                                .unwrap_or(0);
                            let page_num = match page_keys.get(&(app_id.clone(), page_id)) {
                                Some(n) => *n,
                                None => {
                                    let n = next_page_num;
                                    next_page_num = next_page_num.saturating_add(1);
                                    page_keys.insert((app_id.clone(), page_id), n);
                                    n
                                }
                            };
                            let app = apps.get(&app_id);
                            pages.insert(
                                page_num,
                                PageInfo {
                                    page_num,
                                    page_id,
                                    app_id: app_id.clone(),
                                    app_name: app
                                        .map(|a| a.name.clone())
                                        .unwrap_or_else(|| app_id.clone()),
                                    bundle_id: app.map(|a| a.bundle_id.clone()).unwrap_or_default(),
                                    title: get_str(page, "WIRTitleKey").unwrap_or("").to_string(),
                                    url: get_str(page, "WIRURLKey").unwrap_or("").to_string(),
                                },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut page_list: Vec<PageInfo> = pages.into_values().collect();
    page_list.sort_by_key(|page| page.page_num);
    let info = device.info().await;
    Ok((info, page_list))
}
