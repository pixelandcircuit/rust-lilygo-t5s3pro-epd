//! embedded-inspect WebSocket debug server on ESP32-S3.
//!
//! Connects to WiFi, exposes AppState through embedded-inspect via a WebSocket
//! server on port 3000.  Point a browser at http://<device-ip>:3000 to see the
//! live schema tree and field values.
//!
//! Build:
//!   export WIFI_SSID=MyNetwork WIFI_PASS=secret
//!   cargo run --example inspect_demo

#![no_std]
#![no_main]

extern crate alloc;
use alloc::{format, string::String, vec, vec::Vec};
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Runner, Stack, StackResources};
use embassy_sync::{
    blocking_mutex::{raw::CriticalSectionRawMutex, Mutex},
    channel::Channel,
};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, timer::timg::TimerGroup,
};
use esp_radio::wifi::{sta::StationConfig, Config, ControllerConfig, Interface, WifiController};
use static_cell::StaticCell;

use embedded_inspect::{DebugInspect, DebugValue, Inspect, TypeSchema, ValueKind};
use embedded_websocket::{
    WebSocketCloseStatusCode, WebSocketReceiveMessageType, WebSocketSendMessageType,
    WebSocketServer,
};
use epaper::driver::display::{Display, DrawMode};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: StaticCell<$t> = StaticCell::new();
        STATIC_CELL.uninit().write(($val))
    }};
}

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "SSID",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "PASSWORD",
};
const PORT: u16 = 3000;

const INDEX_HTML: &[u8] = include_bytes!("assets/inspect_index.html");

// ── AppState ──────────────────────────────────────────────────────────────────

#[derive(DebugInspect, Default, Clone)]
struct NetworkState {
    #[inspect(read_only)]
    connected: bool,
    #[inspect(read_only)]
    ip_a: u8,
    #[inspect(read_only)]
    ip_b: u8,
    #[inspect(read_only)]
    ip_c: u8,
    #[inspect(read_only)]
    ip_d: u8,
    #[inspect(read_only)]
    rssi: i8,
}

#[derive(DebugInspect, Default, Clone)]
struct DisplayInfo {
    #[inspect(read_only)]
    refresh_count: u32,
    #[inspect(read_only)]
    power_on: bool,
}

#[derive(DebugInspect, Default, Clone)]
struct SystemInfo {
    #[inspect(read_only)]
    uptime_secs: u32,
    #[inspect(read_only)]
    free_heap: u32,
}

#[derive(DebugInspect, Default, Clone)]
struct ContentState {
    #[inspect(read_only)]
    current_page: u32,
    #[inspect(read_only)]
    touch_x: u16,
    #[inspect(read_only)]
    touch_y: u16,
}

#[derive(DebugInspect, Default, Clone)]
struct AppState {
    #[inspect(read_only)]
    network: NetworkState,
    #[inspect(read_only)]
    display: DisplayInfo,
    #[inspect(read_only)]
    system: SystemInfo,
    #[inspect(read_only)]
    content: ContentState,
}

type SharedState = Mutex<CriticalSectionRawMutex, RefCell<AppState>>;
static STATE: StaticCell<SharedState> = StaticCell::new();

// ── Framebuffer snapshot ───────────────────────────────────────────────────────
//
// Synchronization strategy (see DESIGN.md §Screenshot):
//   main copies 259 KB to PSRAM Vec OUTSIDE any lock (interrupts stay enabled,
//   ~3 ms at 80 MB/s), then moves the pointer into the Option under a brief
//   critical section (pointer swap only, ~µs).
//   The debug task reads 4 KB chunks at a time, each under a ~50 µs lock,
//   so rendering is never blocked during network I/O.

const FB_WIDTH: u16 = Display::WIDTH;
const FB_HEIGHT: u16 = Display::HEIGHT;
const CHUNK_SIZE: usize = 4096;

struct FramebufferSnapshot {
    data: Vec<u8>,
    capture_id: u32,
}

type FbState = Mutex<CriticalSectionRawMutex, RefCell<Option<FramebufferSnapshot>>>;
static FB_STATE: StaticCell<FbState> = StaticCell::new();

// ── Structured logging ─────────────────────────────────────────────────────────
//
// ilog!(level, target, …) always writes to USB/serial AND non-blocking enqueues
// into LOG_CHANNEL for WebSocket delivery.  If the channel is full, the record
// is dropped and DROPPED_COUNT is incremented; the debug task reports the count
// to the browser in the next successfully delivered record.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

struct LogEntry {
    timestamp_ms: u64,
    level: LogLevel,
    target: String,
    message: String,
    // dropped_before is filled in by the debug task when it drains the channel
}

const LOG_CAPACITY: usize = 64;
type LogChannel = Channel<CriticalSectionRawMutex, LogEntry, LOG_CAPACITY>;
// const-initialised global — no StaticCell needed since Channel::new() is const fn
static LOG_CHANNEL: LogChannel = LogChannel::new();
static DROPPED_COUNT: AtomicU32 = AtomicU32::new(0);

macro_rules! ilog {
    ($level:expr, $target:expr, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        // Always preserve USB/serial output.
        esp_println::println!("[{}] {}: {}", log_level_str($level), $target, msg);
        let entry = LogEntry {
            timestamp_ms: embassy_time::Instant::now().as_millis(),
            level:        $level,
            target:       alloc::string::String::from($target),
            message:      msg,
        };
        if LOG_CHANNEL.try_send(entry).is_err() {
            DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }};
}

// ── I/O helper ────────────────────────────────────────────────────────────────

// embassy-net's TcpSocket::write may return fewer bytes than requested.
async fn write_all(sock: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        match sock.write(data).await {
            Ok(0) => return Err(()),
            Ok(n) => data = &data[n..],
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

// ── JSON schema tree ──────────────────────────────────────────────────────────

enum SchemaNode {
    Struct {
        type_name: String,
        fields: Vec<FieldNode>,
    },
    Enum {
        type_name: String,
        variants: Vec<String>,
    },
    Primitive {
        primitive: String,
    },
}

struct FieldNode {
    name: String,
    read_only: bool,
    schema: SchemaNode,
}

fn build_schema(inspect: &dyn Inspect) -> SchemaNode {
    match inspect.type_schema() {
        TypeSchema::Struct(s) => {
            let fields = s
                .fields
                .iter()
                .map(|f| {
                    let schema = if f.kind == ValueKind::Object {
                        inspect
                            .get_field(f.name)
                            .and_then(|v| {
                                if let DebugValue::Object(sub) = v {
                                    Some(build_schema(sub))
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(SchemaNode::Primitive {
                                primitive: "object".into(),
                            })
                    } else {
                        SchemaNode::Primitive {
                            primitive: format!("{:?}", f.kind).to_lowercase(),
                        }
                    };
                    FieldNode {
                        name: f.name.into(),
                        read_only: f.read_only,
                        schema,
                    }
                })
                .collect();
            SchemaNode::Struct {
                type_name: s.type_name.into(),
                fields,
            }
        }
        TypeSchema::Enum(e) => SchemaNode::Enum {
            type_name: e.type_name.into(),
            variants: e.variants.iter().map(|v| String::from(*v)).collect(),
        },
    }
}

fn schema_to_json(node: &SchemaNode) -> String {
    match node {
        SchemaNode::Struct { type_name, fields } => {
            let field_jsons: Vec<String> = fields
                .iter()
                .map(|f| {
                    format!(
                        r#"{{"name":"{}","read_only":{},"schema":{}}}"#,
                        f.name,
                        f.read_only,
                        schema_to_json(&f.schema)
                    )
                })
                .collect();
            format!(
                r#"{{"kind":"Struct","type_name":"{}","fields":[{}]}}"#,
                type_name,
                field_jsons.join(",")
            )
        }
        SchemaNode::Enum {
            type_name,
            variants,
        } => {
            let var_jsons: Vec<String> = variants.iter().map(|v| format!("\"{}\"", v)).collect();
            format!(
                r#"{{"kind":"Enum","type_name":"{}","variants":[{}]}}"#,
                type_name,
                var_jsons.join(",")
            )
        }
        SchemaNode::Primitive { primitive } => {
            format!(r#"{{"kind":"Primitive","primitive":"{}"}}"#, primitive)
        }
    }
}

fn debug_value_to_json(v: DebugValue<'_>) -> String {
    match v {
        DebugValue::Bool(b) => format!("{}", b),
        DebugValue::U8(n) => format!("{}", n),
        DebugValue::U16(n) => format!("{}", n),
        DebugValue::U32(n) => format!("{}", n),
        DebugValue::U64(n) => format!("{}", n),
        DebugValue::U128(n) => format!("\"{}\"", n),
        DebugValue::I8(n) => format!("{}", n),
        DebugValue::I16(n) => format!("{}", n),
        DebugValue::I32(n) => format!("{}", n),
        DebugValue::I64(n) => format!("{}", n),
        DebugValue::I128(n) => format!("\"{}\"", n),
        DebugValue::F32(n) => format!("{}", n),
        DebugValue::F64(n) => format!("{}", n),
        DebugValue::Str(s) => format!("\"{}\"", s),
        DebugValue::Object(o) => match o.type_schema() {
            TypeSchema::Enum(_) => format!(
                "{{\"variant\":\"{}\"}}",
                o.active_variant().unwrap_or("unknown")
            ),
            TypeSchema::Struct(s) => format!("{{\"object\":\"{}\"}}", s.type_name),
        },
    }
}

fn collect_leaf_paths(node: &SchemaNode, prefix: &str, paths: &mut Vec<String>) {
    match node {
        SchemaNode::Struct { fields, .. } => {
            for f in fields {
                let path = if prefix.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}.{}", prefix, f.name)
                };
                collect_leaf_paths(&f.schema, &path, paths);
            }
        }
        _ => {
            if !prefix.is_empty() {
                paths.push(String::from(prefix));
            }
        }
    }
}

// ── Minimal JSON field extraction ─────────────────────────────────────────────

fn json_str_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\":\"", key);
    let start = json.find(needle.as_str())? + needle.len();
    let len = json[start..].find('"')?;
    Some(&json[start..start + len])
}

fn json_u32_field(json: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{}\":", key);
    let rest = &json[json.find(needle.as_str())? + needle.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ── Protocol messages ─────────────────────────────────────────────────────────

enum InMsg<'a> {
    Hello { request_id: u32 },
    GetSchema { request_id: u32 },
    GetValue { request_id: u32, path: &'a str },
    GetScreenshot { request_id: u32 },
    TriggerLog { request_id: u32 },
    Unknown,
}

fn parse_msg(json: &str) -> InMsg<'_> {
    let request_id = json_u32_field(json, "request_id").unwrap_or(0);
    match json_str_field(json, "type") {
        Some("Hello") => InMsg::Hello { request_id },
        Some("GetSchema") => InMsg::GetSchema { request_id },
        Some("GetValue") => InMsg::GetValue {
            request_id,
            path: json_str_field(json, "path").unwrap_or(""),
        },
        Some("GetScreenshot") => InMsg::GetScreenshot { request_id },
        Some("TriggerLog") => InMsg::TriggerLog { request_id },
        _ => InMsg::Unknown,
    }
}

fn hello_ack(rid: u32) -> String {
    format!(
        r#"{{"type":"HelloAck","request_id":{},"version":1,"server_name":"inspect-esp32"}}"#,
        rid
    )
}
fn schema_resp(rid: u32, schema: &str) -> String {
    format!(
        r#"{{"type":"SchemaResponse","request_id":{},"schema":{}}}"#,
        rid, schema
    )
}
fn value_resp(rid: u32, path: &str, value: &str) -> String {
    format!(
        r#"{{"type":"ResponseValue","request_id":{},"path":"{}","value":{}}}"#,
        rid, path, value
    )
}
fn error_resp(rid: u32, code: &str, msg: &str) -> String {
    format!(
        r#"{{"type":"Error","request_id":{},"code":"{}","message":"{}"}}"#,
        rid, code, msg
    )
}
fn changed_resp(path: &str, value: &str, seq: u32) -> String {
    format!(
        r#"{{"type":"ValueChanged","path":"{}","value":{},"sequence":{}}}"#,
        path, value, seq
    )
}
fn screenshot_begin_resp(
    capture_id: u32,
    total_bytes: u32,
    chunk_size: u16,
    total_chunks: u32,
) -> String {
    format!(
        r#"{{"type":"ScreenshotBegin","capture_id":{},"width":{},"height":{},"format":"Gray4","total_bytes":{},"chunk_size":{},"total_chunks":{}}}"#,
        capture_id, FB_WIDTH, FB_HEIGHT, total_bytes, chunk_size, total_chunks,
    )
}
fn screenshot_end_resp(capture_id: u32, total_chunks: u32, total_checksum: u32) -> String {
    format!(
        r#"{{"type":"ScreenshotEnd","capture_id":{},"total_chunks":{},"total_checksum":{}}}"#,
        capture_id, total_chunks, total_checksum,
    )
}
fn screenshot_unavailable_resp(rid: u32) -> String {
    error_resp(
        rid,
        "NoSnapshot",
        "no framebuffer snapshot available yet; wait for the next render cycle",
    )
}

fn wrapping_checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
}

fn log_level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

// Escapes a string for embedding inside a JSON "…" value.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use core::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn log_entry_json(entry: &LogEntry, dropped_before: u32) -> String {
    format!(
        r#"{{"type":"LogRecord","timestamp_ms":{},"level":"{}","target":"{}","message":"{}","dropped_before":{}}}"#,
        entry.timestamp_ms,
        log_level_str(entry.level),
        json_escape(&entry.target),
        json_escape(&entry.message),
        dropped_before,
    )
}

// Copies the current framebuffer to a PSRAM snapshot outside any lock (~3 ms at
// 80 MB/s), then moves the pointer into FB_STATE under a brief critical section
// (~µs).  Must be called after render_status() and before flush(BlackOnWhite).
fn capture_framebuffer(display: &mut Display, fb_state: &'static FbState, capture_id: &mut u32) {
    let data = Vec::from(display.framebuffer());
    *capture_id = capture_id.wrapping_add(1);
    let id = *capture_id;
    fb_state.lock(|cell| {
        *cell.borrow_mut() = Some(FramebufferSnapshot {
            data,
            capture_id: id,
        });
    });
    esp_println::println!("[inspect] framebuffer captured (id={})", id);
}

// Streams a screenshot to the connected browser client.
// Returns false if the socket died during the transfer.
async fn send_screenshot(
    ws: &mut WebSocketServer,
    sock: &mut TcpSocket<'_>,
    fb_state: &'static FbState,
    request_id: u32,
    tx_buf: &mut Vec<u8>,
) -> bool {
    let meta = fb_state.lock(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| (s.capture_id, s.data.len() as u32))
    });

    let (capture_id, total_bytes) = match meta {
        None => {
            let msg = screenshot_unavailable_resp(request_id);
            let n = ws
                .write(WebSocketSendMessageType::Text, true, msg.as_bytes(), tx_buf)
                .unwrap_or(0);
            return write_all(sock, &tx_buf[..n]).await.is_ok();
        }
        Some(m) => m,
    };

    let total_chunks = ((total_bytes as usize + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32;

    let begin = screenshot_begin_resp(capture_id, total_bytes, CHUNK_SIZE as u16, total_chunks);
    let n = ws
        .write(
            WebSocketSendMessageType::Text,
            true,
            begin.as_bytes(),
            tx_buf,
        )
        .unwrap_or(0);
    if write_all(sock, &tx_buf[..n]).await.is_err() {
        return false;
    }

    esp_println::println!(
        "[inspect] screenshot: streaming {} chunks for capture_id={}",
        total_chunks,
        capture_id
    );

    // Binary frame payload: 8-byte header (capture_id LE + chunk_index LE) + pixel data.
    let mut pixel_frame = vec![0u8; CHUNK_SIZE + 8];
    // WebSocket-encoded output buffer (server frames are unmasked; max framing overhead = 4 bytes).
    let mut ws_frame = vec![0u8; CHUNK_SIZE + 8 + 16];
    let mut total_checksum: u32 = 0;

    for chunk_index in 0..total_chunks {
        let offset = chunk_index as usize * CHUNK_SIZE;
        let end = (offset + CHUNK_SIZE).min(total_bytes as usize);
        let chunk_len = end - offset;

        pixel_frame[0..4].copy_from_slice(&capture_id.to_le_bytes());
        pixel_frame[4..8].copy_from_slice(&chunk_index.to_le_bytes());

        // Copy chunk data under a brief critical section (~50 µs at 80 MB/s).
        let ok = fb_state.lock(|cell| {
            if let Some(snap) = cell.borrow().as_ref() {
                if snap.capture_id == capture_id {
                    pixel_frame[8..8 + chunk_len].copy_from_slice(&snap.data[offset..end]);
                    return true;
                }
            }
            false
        });
        if !ok {
            // Snapshot was replaced mid-transfer; browser will detect missing ScreenshotEnd.
            esp_println::println!("[inspect] screenshot: snapshot replaced mid-transfer, aborting");
            return true;
        }

        total_checksum =
            total_checksum.wrapping_add(wrapping_checksum(&pixel_frame[8..8 + chunk_len]));

        let n = ws
            .write(
                WebSocketSendMessageType::Binary,
                true,
                &pixel_frame[..8 + chunk_len],
                &mut ws_frame,
            )
            .unwrap_or(0);
        if write_all(sock, &ws_frame[..n]).await.is_err() {
            return false;
        }
    }

    let end_msg = screenshot_end_resp(capture_id, total_chunks, total_checksum);
    let n = ws
        .write(
            WebSocketSendMessageType::Text,
            true,
            end_msg.as_bytes(),
            tx_buf,
        )
        .unwrap_or(0);
    let ok = write_all(sock, &tx_buf[..n]).await.is_ok();
    if ok {
        esp_println::println!("[inspect] screenshot: done (checksum={})", total_checksum);
    }
    ok
}

// ── Embassy tasks ─────────────────────────────────────────────────────────────

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                controller.wait_for_disconnect_async().await.ok();
                esp_println::println!("[inspect] WiFi disconnected, retrying...");
            }
            Err(e) => {
                esp_println::println!("[inspect] connect failed: {:?}", e);
            }
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn debug_server(
    stack: Stack<'static>,
    state: &'static SharedState,
    fb_state: &'static FbState,
) {
    let mut rx_storage = vec![0u8; 2048];
    let mut tx_storage = vec![0u8; 2048];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_storage, &mut tx_storage);
        socket.set_timeout(Some(Duration::from_secs(60)));

        esp_println::println!("[inspect] waiting on port {}", PORT);
        if socket.accept(PORT).await.is_err() {
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }

        ilog!(LogLevel::Info, "inspect", "client connected");
        handle_connection(&mut socket, state, fb_state).await;

        socket.close();
        socket.flush().await.ok();
        socket.abort();
        ilog!(LogLevel::Info, "inspect", "client disconnected");
    }
}

// ── HTTP + WebSocket connection handler ───────────────────────────────────────

async fn handle_connection(
    socket: &mut TcpSocket<'_>,
    state: &'static SharedState,
    fb_state: &'static FbState,
) {
    let mut http_buf = vec![0u8; 1536];
    let mut http_len = 0usize;

    // Read until end-of-headers marker.
    loop {
        match with_timeout(
            Duration::from_secs(10),
            socket.read(&mut http_buf[http_len..]),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
            Ok(Ok(n)) => {
                http_len += n;
                if http_buf[..http_len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if http_len >= http_buf.len() {
                    return;
                }
            }
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 24];
    let mut request = httparse::Request::new(&mut headers);
    if request.parse(&http_buf[..http_len]).is_err() {
        return;
    }

    match embedded_websocket::read_http_header(request.headers.iter().map(|h| (h.name, h.value))) {
        Ok(Some(ctx)) => {
            let mut ws = WebSocketServer::new_server();
            let mut resp_buf = vec![0u8; 512];
            let n = match ws.server_accept(&ctx.sec_websocket_key, None, &mut resp_buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            write_all(socket, &resp_buf[..n]).await.ok();
            ilog!(LogLevel::Debug, "inspect", "WebSocket upgrade OK");
            run_ws_session(&mut ws, socket, state, fb_state).await;
        }
        _ => {
            // Plain HTTP — serve browser UI.
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                INDEX_HTML.len()
            );
            write_all(socket, header.as_bytes()).await.ok();
            write_all(socket, INDEX_HTML).await.ok();
        }
    }
}

// ── WebSocket session ─────────────────────────────────────────────────────────

async fn run_ws_session(
    ws: &mut WebSocketServer,
    sock: &mut TcpSocket<'_>,
    state: &'static SharedState,
    fb_state: &'static FbState,
) {
    // Schema is purely structural ('static field metadata) — build from default
    // values so we never hold the critical section during recursive allocation.
    let schema = build_schema(&AppState::default());
    let schema_json = schema_to_json(&schema);

    let mut leaf_paths: Vec<String> = Vec::new();
    collect_leaf_paths(&schema, "", &mut leaf_paths);
    let mut snapshot: Vec<String> = leaf_paths.iter().map(|_| String::new()).collect();
    let mut seq: u32 = 0;

    let mut rx_buf = vec![0u8; 2048];
    let mut pl_buf = vec![0u8; 1536];
    let mut tx_buf = vec![0u8; 2048];
    let mut buf_used = 0usize;

    let mut last_event = Instant::now();

    'outer: loop {
        // Drain log channel — best-effort, non-blocking.
        // Each iteration: atomically claim the accumulated dropped count and
        // embed it in the record so the browser sees exactly how many were lost.
        loop {
            match LOG_CHANNEL.try_receive() {
                Ok(entry) => {
                    let dropped = DROPPED_COUNT.swap(0, Ordering::Relaxed);
                    let json = log_entry_json(&entry, dropped);
                    let n = ws
                        .write(
                            WebSocketSendMessageType::Text,
                            true,
                            json.as_bytes(),
                            &mut tx_buf,
                        )
                        .unwrap_or(0);
                    if n > 0 && write_all(sock, &tx_buf[..n]).await.is_err() {
                        break 'outer;
                    }
                }
                Err(_) => break,
            }
        }

        // Push ValueChanged events every 2 s.
        // Take one snapshot under a single brief critical section, then do all
        // comparison and I/O outside the lock so interrupts stay enabled.
        if Instant::now() - last_event >= Duration::from_secs(2) {
            let snap: AppState = state.lock(|cell| cell.borrow().clone());
            for (i, path) in leaf_paths.iter().enumerate() {
                if let Some(val) = snap.get_field_path(path).map(debug_value_to_json) {
                    if snapshot[i] != val {
                        snapshot[i] = val.clone();
                        let msg = changed_resp(path, &val, seq);
                        seq += 1;
                        let n = ws
                            .write(
                                WebSocketSendMessageType::Text,
                                true,
                                msg.as_bytes(),
                                &mut tx_buf,
                            )
                            .unwrap_or(0);
                        if n > 0 && write_all(sock, &tx_buf[..n]).await.is_err() {
                            break 'outer;
                        }
                    }
                }
            }
            last_event = Instant::now();
        }

        let elapsed = Instant::now() - last_event;
        let remaining = if elapsed < Duration::from_secs(2) {
            Duration::from_secs(2) - elapsed
        } else {
            Duration::from_millis(20)
        };

        match with_timeout(remaining, sock.read(&mut rx_buf[buf_used..])).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Err(_timeout) => continue,
            Ok(Ok(n)) => buf_used += n,
        }

        'inner: loop {
            if buf_used == 0 {
                break 'inner;
            }
            match ws.read(&rx_buf[..buf_used], &mut pl_buf) {
                Err(_) => break 'outer,
                Ok(r) if r.len_from == 0 => break 'inner,
                Ok(r) => {
                    // Shift buffer before any await so we don't borrow rx_buf across awaits.
                    let consumed = r.len_from;
                    rx_buf.copy_within(consumed..buf_used, 0);
                    buf_used -= consumed;
                    let payload = &pl_buf[..r.len_to];

                    match r.message_type {
                        WebSocketReceiveMessageType::Text => {
                            match core::str::from_utf8(payload) {
                                Ok(json) => match parse_msg(json) {
                                    InMsg::GetScreenshot { request_id } => {
                                        ilog!(
                                            LogLevel::Debug,
                                            "inspect",
                                            "GetScreenshot request_id={}",
                                            request_id
                                        );
                                        if !send_screenshot(
                                            ws,
                                            sock,
                                            fb_state,
                                            request_id,
                                            &mut tx_buf,
                                        )
                                        .await
                                        {
                                            break 'outer;
                                        }
                                        // After async work, break to outer loop to re-poll TCP.
                                        break 'inner;
                                    }
                                    _ => {
                                        let reply = handle_msg(json, state, &schema_json);
                                        let n = ws
                                            .write(
                                                WebSocketSendMessageType::Text,
                                                true,
                                                reply.as_bytes(),
                                                &mut tx_buf,
                                            )
                                            .unwrap_or(0);
                                        if write_all(sock, &tx_buf[..n]).await.is_err() {
                                            break 'outer;
                                        }
                                    }
                                },
                                Err(_) => {
                                    let msg = error_resp(0, "BadEncoding", "non-UTF-8");
                                    let n = ws
                                        .write(
                                            WebSocketSendMessageType::Text,
                                            true,
                                            msg.as_bytes(),
                                            &mut tx_buf,
                                        )
                                        .unwrap_or(0);
                                    if write_all(sock, &tx_buf[..n]).await.is_err() {
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        WebSocketReceiveMessageType::CloseMustReply => {
                            let n = ws
                                .close(WebSocketCloseStatusCode::NormalClosure, None, &mut tx_buf)
                                .unwrap_or(0);
                            write_all(sock, &tx_buf[..n]).await.ok();
                            break 'outer;
                        }
                        WebSocketReceiveMessageType::Ping => {
                            let n = ws
                                .write(WebSocketSendMessageType::Pong, true, payload, &mut tx_buf)
                                .unwrap_or(0);
                            if write_all(sock, &tx_buf[..n]).await.is_err() {
                                break 'outer;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn handle_msg(json: &str, state: &'static SharedState, schema_json: &str) -> String {
    match parse_msg(json) {
        InMsg::Hello { request_id } => {
            ilog!(
                LogLevel::Info,
                "inspect",
                "Hello request_id={} — sending HelloAck",
                request_id
            );
            hello_ack(request_id)
        }
        InMsg::GetSchema { request_id } => schema_resp(request_id, schema_json),
        InMsg::GetValue { request_id, path } => {
            let result = state.lock(|cell| {
                let app = cell.borrow();
                app.get_field_path(path).map(debug_value_to_json)
            });
            match result {
                Some(v) => value_resp(request_id, path, &v),
                None => error_resp(
                    request_id,
                    "UnknownPath",
                    &format!("no field at path: {}", path),
                ),
            }
        }
        InMsg::TriggerLog { request_id } => {
            ilog!(LogLevel::Info, "inspect", "log triggered from browser (request_id={})", request_id);
            ilog!(LogLevel::Debug, "inspect", "uptime_ms={}", embassy_time::Instant::now().as_millis());
            ilog!(LogLevel::Warn, "inspect", "this is a sample warn message");
            format!(r#"{{"type":"TriggerLogAck","request_id":{}}}"#, request_id)
        }
        InMsg::GetScreenshot { .. } | InMsg::Unknown => {
            error_resp(0, "UnknownMessage", "unrecognised message type")
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    esp_alloc::heap_allocator!(size: 72 * 1024);

    esp_println::logger::init_logger_from_env();

    let state: &'static SharedState = STATE.init(Mutex::new(RefCell::new(AppState::default())));
    let fb_state: &'static FbState = FB_STATE.init(Mutex::new(RefCell::new(None)));

    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");
    display.power_on();
    state.lock(|cell| cell.borrow_mut().display.power_on = true);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    let station_cfg = Config::Station(
        StationConfig::default()
            .with_ssid(SSID)
            .with_password(PASSWORD.into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_cfg),
    )
    .expect("wifi init");

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        0x1234_5678_u64,
    );

    spawner.spawn(net_task(runner).expect("net_task"));
    spawner.spawn(connection(controller).expect("connection"));
    spawner.spawn(debug_server(stack, state, fb_state).expect("debug_server"));

    ilog!(LogLevel::Info, "wifi", "connecting to '{}'...", SSID);
    stack.wait_config_up().await;

    if let Some(cfg) = stack.config_v4() {
        let ip = cfg.address.address();
        let oct = ip.octets();
        ilog!(
            LogLevel::Info,
            "wifi",
            "connected — http://{}.{}.{}.{}:{}/",
            oct[0],
            oct[1],
            oct[2],
            oct[3],
            PORT
        );
        state.lock(|cell| {
            let mut s = cell.borrow_mut();
            s.network.connected = true;
            s.network.ip_a = oct[0];
            s.network.ip_b = oct[1];
            s.network.ip_c = oct[2];
            s.network.ip_d = oct[3];
        });
    }

    let boot = Instant::now();
    let mut refreshes = 0u32;
    let mut last_draw = 0u32;
    let mut fb_capture_id: u32 = 0;

    loop {
        let uptime = (Instant::now() - boot).as_secs() as u32;

        state.lock(|cell| {
            let mut s = cell.borrow_mut();
            s.system.uptime_secs = uptime;
            s.network.connected = stack.is_link_up();
        });

        if uptime == 0 || uptime - last_draw >= 30 {
            last_draw = uptime;
            ilog!(
                LogLevel::Debug,
                "render",
                "refresh #{} at uptime={}s",
                refreshes + 1,
                uptime
            );
            // Pass 1: drive all pixels to white so old QR/text doesn't ghost.
            display.fill(0xF).unwrap();
            display.flush(DrawMode::WhiteOnBlack).unwrap();
            // Pass 2: render new content.
            render_status(&mut display, state);
            // Capture framebuffer snapshot BEFORE flush resets it to 0xFF.
            capture_framebuffer(&mut display, fb_state, &mut fb_capture_id);
            display.flush(DrawMode::BlackOnWhite).unwrap();
            refreshes += 1;
            state.lock(|cell| cell.borrow_mut().display.refresh_count = refreshes);
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}

fn render_status(display: &mut Display, state: &'static SharedState) {
    use embedded_graphics::{
        geometry::Point,
        mono_font::{ascii::FONT_9X18, MonoTextStyle},
        pixelcolor::{Gray4, GrayColor},
        text::{Alignment, Text},
        Drawable,
    };

    display.fill(0xF).unwrap();

    let black = Gray4::BLACK;
    let style = MonoTextStyle::new(&FONT_9X18, black);

    let (connected, ip_a, ip_b, ip_c, ip_d, uptime) = state.lock(|cell| {
        let s = cell.borrow();
        (
            s.network.connected,
            s.network.ip_a,
            s.network.ip_b,
            s.network.ip_c,
            s.network.ip_d,
            s.system.uptime_secs,
        )
    });

    Text::with_alignment(
        "embedded-inspect demo",
        Point::new(480, 40),
        style,
        Alignment::Center,
    )
    .draw(display)
    .unwrap();

    let addr = if connected {
        format!("http://{}.{}.{}.{}:{}/", ip_a, ip_b, ip_c, ip_d, PORT)
    } else {
        String::from("connecting to WiFi...")
    };
    Text::with_alignment(&addr, Point::new(480, 80), style, Alignment::Center)
        .draw(display)
        .unwrap();

    let up = format!("uptime: {}s", uptime);
    Text::with_alignment(&up, Point::new(480, 120), style, Alignment::Center)
        .draw(display)
        .unwrap();

    if connected {
        let qr_url = format!("http://{}.{}.{}.{}:{}/", ip_a, ip_b, ip_c, ip_d, PORT);
        render_qr(display, &qr_url);
    }
}

fn render_qr(display: &mut Display, url: &str) {
    use embedded_graphics::{
        geometry::{Point, Size},
        pixelcolor::{Gray4, GrayColor},
        primitives::{Primitive, PrimitiveStyle, Rectangle},
        Drawable,
    };
    use qrcode_core::{
        bits::encode_auto,
        canvas::Canvas,
        ec::construct_codewords,
        types::{Color, EcLevel},
    };

    let ec = EcLevel::M;
    let Ok(bits) = encode_auto(url.as_bytes(), ec) else {
        return;
    };
    let version = bits.version();
    let Ok((data_cw, ec_cw)) = construct_codewords(&bits.into_bytes(), version, ec) else {
        return;
    };

    let mut canvas = Canvas::new(version, ec);
    canvas.draw_all_functional_patterns();
    canvas.draw_data(&data_cw, &ec_cw);
    let canvas = canvas.apply_best_mask();
    let colors = canvas.into_colors();

    let width = version.width() as i32;
    let quiet = 4i32;
    let scale = 8i32;
    let total = (width + quiet * 2) * scale;
    let x0 = (960 - total) / 2;
    let y0 = 160i32;
    let dark = PrimitiveStyle::with_fill(Gray4::BLACK);

    for row in 0..width {
        for col in 0..width {
            if colors[(row * width + col) as usize] == Color::Dark {
                Rectangle::new(
                    Point::new(x0 + (col + quiet) * scale, y0 + (row + quiet) * scale),
                    Size::new(scale as u32, scale as u32),
                )
                .into_styled(dark)
                .draw(display)
                .ok();
            }
        }
    }
}
