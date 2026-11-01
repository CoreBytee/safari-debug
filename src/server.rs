use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::device::{DeviceHandle, DeviceManager, WsClient};
use crate::frontend::Frontend;
use crate::webinspector::MAX_MESSAGE_SIZE;

const ADDITIONS_CSS: &str = include_str!("../assets/additions.css");
const ADDITIONS_JS: &str = include_str!("../assets/additions.js");

pub struct AppState {
    pub manager: Arc<DeviceManager>,
    pub frontend: Arc<Frontend>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/devices", get(api_devices))
        .route("/api/refresh", post(api_refresh))
        .route("/json", get(json_devices))
        .route("/json/list", get(json_pages))
        .route("/inspector/{udid}/{page}", get(inspector_redirect))
        .route("/inspector/{udid}/{page}/", get(inspector_index))
        .route("/inspector/{udid}/{page}/{*path}", get(inspector_asset))
        .route("/ws/{udid}/{page}", get(ws_handler))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn api_devices(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let devices = state.manager.snapshot().await;
    let usbmuxd = state.manager.usbmuxd_status().await;
    Json(json!({ "devices": devices, "usbmuxd": usbmuxd }))
}

async fn api_refresh(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    state.manager.refresh_all().await;
    Json(json!({ "ok": true }))
}

async fn json_devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let host = host_from_headers(&headers);
    let devices: Vec<_> = state
        .manager
        .snapshot()
        .await
        .into_iter()
        .map(|device| {
            json!({
                "deviceId": device.udid,
                "deviceName": device.name,
                "deviceOSVersion": device.os_version,
                "url": host,
                "connected": device.connected,
                "status": device.status,
            })
        })
        .collect();
    Json(json!(devices))
}

async fn json_pages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let host = host_from_headers(&headers);
    let mut pages = Vec::new();
    for device in state.manager.snapshot().await {
        for page in device.pages {
            pages.push(json!({
                "id": format!("{}/{}", device.udid, page.page_num),
                "type": "page",
                "title": page.title,
                "url": page.url,
                "description": page.app_name,
                "webSocketDebuggerUrl": format!(
                    "ws://{host}/ws/{}/{}",
                    device.udid, page.page_num
                ),
            }));
        }
    }
    Json(json!(pages))
}

fn host_from_headers(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1:9223")
        .to_string()
}

async fn inspector_redirect(uri: Uri) -> Redirect {
    let path = format!("{}/", uri.path().trim_end_matches('/'));
    let location = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    Redirect::permanent(&location)
}

async fn inspector_index(
    State(state): State<Arc<AppState>>,
    AxumPath((udid, page)): AxumPath<(String, u32)>,
) -> Response {
    let Some(device) = state.manager.get(&udid).await else {
        return not_found("This device is no longer connected.");
    };

    if !device.pages().await.iter().any(|p| p.page_num == page) {
        return not_found("This page is no longer open in Safari.");
    }

    serve_main_html(&state.frontend).await
}

async fn inspector_asset(
    State(state): State<Arc<AppState>>,
    AxumPath((udid, _page, path)): AxumPath<(String, u32, String)>,
) -> Response {
    let Some(device) = state.manager.get(&udid).await else {
        return not_found("This device is no longer connected.");
    };

    let Some(relative) = safe_relative(&path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };

    let relative_str = relative.to_string_lossy();
    if relative_str == "Protocol/InspectorBackendCommands.js" {
        let version = device.os_version_tuple().await;
        let Some(protocol_path) = state.frontend.protocol_path_for(version) else {
            return not_found("No matching inspector protocol was found.");
        };
        return file_response(protocol_path, "text/javascript; charset=utf-8").await;
    }

    if relative_str == "WebKitAdditions/WebInspectorUI/WebInspectorUIAdditions.css" {
        return static_response(ADDITIONS_CSS.as_bytes(), "text/css; charset=utf-8");
    }
    if relative_str == "WebKitAdditions/WebInspectorUI/WebInspectorUIAdditions.js" {
        return static_response(ADDITIONS_JS.as_bytes(), "text/javascript; charset=utf-8");
    }

    if relative_str.is_empty() || relative_str == "Main.html" {
        return serve_main_html(&state.frontend).await;
    }

    let path = state.frontend.root().join(&relative);
    file_response(&path, content_type_for(&path)).await
}

async fn serve_main_html(frontend: &Frontend) -> Response {
    file_response(
        &frontend.root().join("Main.html"),
        "text/html; charset=utf-8",
    )
    .await
}

async fn file_response(path: &Path, content_type: &str) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => static_response(&bytes, content_type),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => not_found("File not found."),
        Err(error) => {
            warn!("failed to read {}: {error}", path.display());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read file: {error}"),
            )
                .into_response()
        }
    }
}

fn static_response(bytes: &[u8], content_type: &str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        bytes.to_vec(),
    )
        .into_response()
}

fn content_type_for(path: &Path) -> &'static str {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // mime_guess returns a Mime that lives long enough for the common types
    // below. Everything else is served as an octet stream.
    match mime.essence_str() {
        "text/html" => "text/html; charset=utf-8",
        "text/css" => "text/css; charset=utf-8",
        "text/javascript" | "application/javascript" => "text/javascript; charset=utf-8",
        "application/json" => "application/json; charset=utf-8",
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/svg+xml" => "image/svg+xml",
        "image/x-icon" => "image/x-icon",
        "font/woff" => "font/woff",
        "font/woff2" => "font/woff2",
        "application/octet-stream" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn safe_relative(path: &str) -> Option<PathBuf> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Some(PathBuf::from("Main.html"));
    }
    let mut result = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if result.as_os_str().is_empty() {
        Some(PathBuf::from("Main.html"))
    } else {
        Some(result)
    }
}

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        html_escape(message),
    )
        .into_response()
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    AxumPath((udid, page)): AxumPath<(String, u32)>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let Some(device) = state.manager.get(&udid).await else {
        return not_found("This device is no longer connected.");
    };

    if !device.is_connected().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The device is not connected to the WebInspector service yet. Wait a moment and retry.",
        )
            .into_response();
    }

    if !device.pages().await.iter().any(|p| p.page_num == page) {
        return not_found("This page is no longer open in Safari.");
    }

    ws.max_message_size(MAX_MESSAGE_SIZE)
        .max_frame_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| ws_session(socket, device, page))
}

async fn ws_session(socket: WebSocket, device: Arc<DeviceHandle>, page_num: u32) {
    let (mut sink, mut stream) = socket.split();
    let sender_id = Uuid::new_v4().to_string().to_uppercase();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(1024);

    device
        .add_ws_client(
            sender_id.clone(),
            WsClient {
                tx: out_tx,
                page_num,
            },
        )
        .await;

    let setup_ok = device.queue_setup(&sender_id, page_num).await;
    if !setup_ok {
        device.remove_ws_client(&sender_id).await;
        let _ = sink.send(Message::Close(None)).await;
        return;
    }

    debug!("websocket client {sender_id} attached to page {page_num}");

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let data = text.to_string().into_bytes();
                        if !device.queue_data(&sender_id, page_num, data).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if !device.queue_data(&sender_id, page_num, data.to_vec()).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(error)) => {
                        debug!("websocket receive error: {error}");
                        break;
                    }
                }
            }
            outgoing = out_rx.recv() => {
                match outgoing {
                    Some(text) => {
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    device.remove_ws_client(&sender_id).await;
    let _ = device.queue_close(&sender_id, page_num).await;
    let _ = sink.send(Message::Close(None)).await;
    debug!("websocket client {sender_id} detached from page {page_num}");
}
