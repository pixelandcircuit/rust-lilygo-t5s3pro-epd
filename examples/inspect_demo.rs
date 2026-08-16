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
use embassy_net::{
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
    IpAddress, IpEndpoint, Runner, Stack, StackResources,
};
use embassy_sync::{
    blocking_mutex::{raw::CriticalSectionRawMutex, Mutex},
    channel::Channel,
    signal::Signal,
};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, timer::timg::TimerGroup,
};
use esp_radio::wifi::{sta::StationConfig, Config, ControllerConfig, Interface, WifiController};
use static_cell::StaticCell;

use embedded_inspect::{
    debug_commands, CommandArg, CommandDef, CommandOutput, CommandParamKind, CommandReturnKind,
    DebugCommands, DebugInspect, DebugSetValue, DebugValue, Inspect, SetValueResult, TypeSchema,
    ValueKind,
};
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

// ── Device identity ───────────────────────────────────────────────────────────
//
// Set DEVICE_NAME at build time: `DEVICE_NAME="T5 Reader" cargo run ...`
// The name appears in HelloAck, in the mDNS service instance, and in the
// browser header. FIRMWARE_VERSION is taken from Cargo.toml automatically.

const DEVICE_NAME: &str = match option_env!("DEVICE_NAME") {
    Some(s) => s,
    None => "ESP32-S3",
};
const DEVICE_TYPE_STR: &str = "ESP32-S3";
const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    #[inspect(read_only, metric, unit = "dBm")]
    rssi: i8,
}

#[derive(DebugInspect, Default, Clone)]
struct DisplayInfo {
    #[inspect(read_only, metric)]
    refresh_count: u32,
    #[inspect(read_only)]
    power_on: bool,
}

#[derive(DebugInspect, Default, Clone)]
struct SystemInfo {
    #[inspect(read_only, metric, unit = "s")]
    uptime_secs: u32,
    #[inspect(read_only, metric, unit = "B")]
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
    /// Writable: browser can set display brightness (0–100).
    #[inspect(write, metric, unit = "%", min = 0, max = 100)]
    brightness: u8,
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

// ── Remote command dispatch ────────────────────────────────────────────────────
//
// The debug task sends CommandRequest to CMD_CHANNEL and then awaits CMD_RESP.
// The main task drains CMD_CHANNEL each render iteration, dispatches to AppState,
// and signals CMD_RESP with the result.  This keeps mutable AppState ownership
// entirely in main — the debug task never touches it directly.

struct CommandRequest {
    request_id: u32,
    name: String,
    args: Vec<CommandArg>,
}

struct CommandResponse {
    request_id: u32,
    output: CommandOutput,
    duration_ms: u32,
}

type CmdChannel = Channel<CriticalSectionRawMutex, CommandRequest, 4>;
static CMD_CHANNEL: CmdChannel = CmdChannel::new();
static CMD_RESP: Signal<CriticalSectionRawMutex, CommandResponse> = Signal::new();

// ── Remote set-value dispatch ─────────────────────────────────────────────────
//
// Same pattern as CMD_CHANNEL/CMD_RESP: debug task enqueues a SetValueRequest,
// main task drains and applies it via DebugSetValue, signals result back.

struct SetValueRequest {
    request_id: u32,
    path: String,
    value: CommandArg,
}

type SetChannel = Channel<CriticalSectionRawMutex, SetValueRequest, 4>;
static SET_CHANNEL: SetChannel = SetChannel::new();
static SET_RESP: Signal<CriticalSectionRawMutex, (u32, SetValueResult)> = Signal::new();

#[debug_commands]
impl AppState {
    #[debug_command]
    fn reset_refresh_count(&mut self) {
        self.display.refresh_count = 0;
    }

    #[debug_command]
    fn set_content_page(&mut self, page: u32) {
        self.content.current_page = page;
    }
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
    min_val: Option<f64>,
    max_val: Option<f64>,
    metric: bool,
    unit: Option<&'static str>,
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
                        min_val: f.min_val,
                        max_val: f.max_val,
                        metric: f.metric,
                        unit: f.unit,
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
                    let min_json = match f.min_val {
                        Some(v) => format!(",\"min_val\":{}", v),
                        None => String::new(),
                    };
                    let max_json = match f.max_val {
                        Some(v) => format!(",\"max_val\":{}", v),
                        None => String::new(),
                    };
                    let metric_json = if f.metric { ",\"metric\":true" } else { "" };
                    let unit_json = match f.unit {
                        Some(u) => format!(",\"unit\":\"{}\"", u),
                        None => String::new(),
                    };
                    format!(
                        r#"{{"name":"{}","read_only":{}{}{}{}{},"schema":{}}}"#,
                        f.name,
                        f.read_only,
                        min_json,
                        max_json,
                        metric_json,
                        unit_json,
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

fn json_bool_field(json: &str, key: &str) -> bool {
    let needle = format!("\"{}\":", key);
    let rest = match json.find(needle.as_str()) {
        Some(p) => &json[p + needle.len()..],
        None => return false,
    };
    let rest = rest.trim_start();
    rest.starts_with("true")
}

fn json_raw_array_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\":[", key);
    let start = json.find(needle.as_str())? + needle.len() - 1;
    let rest = &json[start..];
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in rest.char_indices() {
        if escape { escape = false; continue; }
        if c == '\\' && in_str { escape = true; continue; }
        if c == '"' { in_str = !in_str; continue; }
        if in_str { continue; }
        if c == '[' { depth += 1; }
        else if c == ']' {
            depth -= 1;
            if depth == 0 { return Some(&rest[..=i]); }
        }
    }
    None
}

fn json_raw_object_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\":{{", key);
    let start = json.find(needle.as_str())? + needle.len() - 1;
    let rest = &json[start..];
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in rest.char_indices() {
        if escape { escape = false; continue; }
        if c == '\\' && in_str { escape = true; continue; }
        if c == '"' { in_str = !in_str; continue; }
        if in_str { continue; }
        if c == '{' { depth += 1; }
        else if c == '}' {
            depth -= 1;
            if depth == 0 { return Some(&rest[..=i]); }
        }
    }
    None
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
    GetCommands { request_id: u32 },
    InvokeCommand { request_id: u32, name: &'a str, args_json: &'a str },
    SetValue { request_id: u32, path: &'a str, value_json: &'a str },
    SubscribeMetrics { request_id: u32, paths_json: &'a str, interval_ms: u32 },
    UnsubscribeMetrics { request_id: u32, paths_json: &'a str },
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
        Some("GetCommands") => InMsg::GetCommands { request_id },
        Some("InvokeCommand") => InMsg::InvokeCommand {
            request_id,
            name: json_str_field(json, "name").unwrap_or(""),
            args_json: json_raw_array_field(json, "args").unwrap_or("[]"),
        },
        Some("SetValue") => InMsg::SetValue {
            request_id,
            path: json_str_field(json, "path").unwrap_or(""),
            value_json: json_raw_object_field(json, "value").unwrap_or("{}"),
        },
        Some("SubscribeMetrics") => InMsg::SubscribeMetrics {
            request_id,
            paths_json: json_raw_array_field(json, "paths").unwrap_or("[]"),
            interval_ms: json_u32_field(json, "interval_ms").unwrap_or(1000),
        },
        Some("UnsubscribeMetrics") => InMsg::UnsubscribeMetrics {
            request_id,
            paths_json: json_raw_array_field(json, "paths").unwrap_or("[]"),
        },
        _ => InMsg::Unknown,
    }
}

fn mdns_hostname_for(ip_c: u8, ip_d: u8) -> String {
    let slug = slugify(DEVICE_NAME);
    format!("{}-{:02x}{:02x}.local", slug, ip_c, ip_d)
}

fn hello_ack(rid: u32, state: &'static SharedState) -> String {
    let (ip_c, ip_d) = state.lock(|cell| {
        let s = cell.borrow();
        (s.network.ip_c, s.network.ip_d)
    });
    let hostname = if ip_c == 0 && ip_d == 0 {
        String::new()
    } else {
        mdns_hostname_for(ip_c, ip_d)
    };
    format!(
        r#"{{"type":"HelloAck","request_id":{},"version":1,"server_name":"inspect-esp32","device_name":"{}","device_type":"{}","firmware_version":"{}","mdns_hostname":"{}"}}"#,
        rid,
        json_escape(DEVICE_NAME),
        DEVICE_TYPE_STR,
        FIRMWARE_VERSION,
        json_escape(&hostname),
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

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = String::from(out.trim_end_matches('-'));
    if trimmed.is_empty() { String::from("device") } else { trimmed }
}

// ── mDNS / DNS-SD ─────────────────────────────────────────────────────────────
//
// Minimal mDNS implementation (RFC 6762 / RFC 6763).  Handles:
//   - Proactive announcements to 224.0.0.251:5353 every 30 s
//   - Responding to A queries for <hostname>.local
//   - Responding to PTR queries for _embedded-inspect._tcp.local
//
// No name compression is used in outgoing packets (always legal per RFC 1035
// §4.1.4).  Incoming pointers are followed when parsing queries.

const MDNS_IP: IpAddress = IpAddress::v4(224, 0, 0, 251);

fn mdns_push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.push((v >> 8) as u8);
    buf.push(v as u8);
}

fn mdns_push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.push((v >> 24) as u8);
    buf.push((v >> 16) as u8);
    buf.push((v >> 8) as u8);
    buf.push(v as u8);
}

fn mdns_encode_name(buf: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        if label.is_empty() { continue; }
        buf.push(label.len() as u8);
        buf.extend_from_slice(label.as_bytes());
    }
    buf.push(0); // root label
}

// Encode a single DNS resource record (no name compression).
// class = 0x8001 (IN | cache-flush bit for mDNS).
fn mdns_rr(buf: &mut Vec<u8>, name: &str, rtype: u16, ttl: u32, rdata: &[u8]) {
    mdns_encode_name(buf, name);
    mdns_push_u16(buf, rtype);      // TYPE
    mdns_push_u16(buf, 0x8001);     // CLASS = IN | cache-flush
    mdns_push_u32(buf, ttl);        // TTL
    mdns_push_u16(buf, rdata.len() as u16); // RDLENGTH
    buf.extend_from_slice(rdata);
}

/// Build a full mDNS announcement: PTR + SRV + TXT + A records.
fn build_mdns_announcement(hostname: &str, instance: &str, ip: [u8; 4]) -> Vec<u8> {
    let mut pkt = Vec::new();
    // DNS header
    mdns_push_u16(&mut pkt, 0);      // ID = 0
    mdns_push_u16(&mut pkt, 0x8400); // Flags: QR=1 (response), AA=1 (authoritative)
    mdns_push_u16(&mut pkt, 0);      // QDCOUNT
    mdns_push_u16(&mut pkt, 4);      // ANCOUNT = 4 records
    mdns_push_u16(&mut pkt, 0);      // NSCOUNT
    mdns_push_u16(&mut pkt, 0);      // ARCOUNT

    let svc_type = "_embedded-inspect._tcp.local";
    let full_instance = format!("{}.{}", instance, svc_type);
    let hostname_local = format!("{}.local", hostname);

    // PTR: _embedded-inspect._tcp.local → full_instance  (TTL 4500 s per RFC 6762)
    {
        let mut rdata = Vec::new();
        mdns_encode_name(&mut rdata, &full_instance);
        mdns_rr(&mut pkt, svc_type, 12, 4500, &rdata);
    }
    // SRV: full_instance → hostname.local:PORT  (TTL 120 s)
    {
        let mut rdata = Vec::new();
        mdns_push_u16(&mut rdata, 0);    // Priority
        mdns_push_u16(&mut rdata, 0);    // Weight
        mdns_push_u16(&mut rdata, PORT); // Port
        mdns_encode_name(&mut rdata, &hostname_local);
        mdns_rr(&mut pkt, &full_instance, 33, 120, &rdata);
    }
    // TXT: full_instance → key=value strings  (TTL 4500 s)
    {
        let mut rdata = Vec::new();
        for entry in &[
            format!("name={}", instance),
            format!("type={}", DEVICE_TYPE_STR),
            format!("version={}", FIRMWARE_VERSION),
            String::from("proto=1"),
        ] {
            rdata.push(entry.len() as u8);
            rdata.extend_from_slice(entry.as_bytes());
        }
        mdns_rr(&mut pkt, &full_instance, 16, 4500, &rdata);
    }
    // A: hostname.local → ip  (TTL 120 s)
    mdns_rr(&mut pkt, &hostname_local, 1, 120, &ip);

    pkt
}

/// Parse a DNS name starting at `offset`; advances `offset` past the name.
/// Returns `None` on malformed input.
fn mdns_parse_name(pkt: &[u8], offset: &mut usize) -> Option<String> {
    let mut name = String::new();
    let mut pos = *offset;
    let mut jumped = false;
    let mut hops = 0usize;

    loop {
        if pos >= pkt.len() { return None; }
        let len = pkt[pos] as usize;

        if len & 0xC0 == 0xC0 {
            // Pointer (name compression)
            if pos + 1 >= pkt.len() { return None; }
            let ptr = ((len & 0x3F) << 8) | pkt[pos + 1] as usize;
            if !jumped { *offset = pos + 2; }
            pos = ptr;
            jumped = true;
            hops += 1;
            if hops > 16 { return None; } // loop guard
            continue;
        }

        if len == 0 {
            if !jumped { *offset = pos + 1; }
            break;
        }

        pos += 1;
        if pos + len > pkt.len() { return None; }
        if !name.is_empty() { name.push('.'); }
        name.push_str(core::str::from_utf8(&pkt[pos..pos + len]).ok()?);
        pos += len;
    }
    Some(name)
}

/// Parse an mDNS query and build a response if any questions match our records.
/// Returns `None` if the packet is not a query or nothing matches.
fn handle_mdns_query(pkt: &[u8], hostname: &str, instance: &str, ip: [u8; 4]) -> Option<Vec<u8>> {
    if pkt.len() < 12 { return None; }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if flags & 0x8000 != 0 { return None; } // Is a response, not a query — ignore

    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    if qdcount == 0 { return None; }

    let svc_type = "_embedded-inspect._tcp.local";
    let hostname_local = format!("{}.local", hostname);

    let mut want_a = false;
    let mut want_ptr = false;
    let mut offset = 12;

    for _ in 0..qdcount {
        let qname = mdns_parse_name(pkt, &mut offset)?;
        if offset + 4 > pkt.len() { return None; }
        let qtype = u16::from_be_bytes([pkt[offset], pkt[offset + 1]]);
        offset += 4; // skip qtype + qclass

        if qname.eq_ignore_ascii_case(&hostname_local) && (qtype == 1 || qtype == 255 || qtype == 28) {
            want_a = true;
        }
        if qname.eq_ignore_ascii_case(svc_type) && (qtype == 12 || qtype == 255) {
            want_ptr = true;
        }
    }

    if !want_a && !want_ptr { return None; }

    // Build response — same structure as announcement but filtered to what was asked.
    let an_count = (want_ptr as u16) * 3 + (want_a as u16);
    let mut resp = Vec::new();
    let id = u16::from_be_bytes([pkt[0], pkt[1]]);
    mdns_push_u16(&mut resp, id);
    mdns_push_u16(&mut resp, 0x8400);
    mdns_push_u16(&mut resp, 0);
    mdns_push_u16(&mut resp, an_count);
    mdns_push_u16(&mut resp, 0);
    mdns_push_u16(&mut resp, 0);

    let full_instance = format!("{}.{}", instance, svc_type);

    if want_ptr {
        let mut rdata = Vec::new();
        mdns_encode_name(&mut rdata, &full_instance);
        mdns_rr(&mut resp, svc_type, 12, 4500, &rdata);

        let mut rdata = Vec::new();
        mdns_push_u16(&mut rdata, 0);
        mdns_push_u16(&mut rdata, 0);
        mdns_push_u16(&mut rdata, PORT);
        mdns_encode_name(&mut rdata, &hostname_local);
        mdns_rr(&mut resp, &full_instance, 33, 120, &rdata);

        let mut rdata = Vec::new();
        for entry in &[
            format!("name={}", instance),
            format!("type={}", DEVICE_TYPE_STR),
            format!("version={}", FIRMWARE_VERSION),
        ] {
            rdata.push(entry.len() as u8);
            rdata.extend_from_slice(entry.as_bytes());
        }
        mdns_rr(&mut resp, &full_instance, 16, 4500, &rdata);
    }
    if want_a {
        mdns_rr(&mut resp, &hostname_local, 1, 120, &ip);
    }
    Some(resp)
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

// ── Command JSON helpers ───────────────────────────────────────────────────────

fn command_param_kind_json(kind: &CommandParamKind) -> String {
    match kind {
        CommandParamKind::Bool => r#"{"type":"bool"}"#.into(),
        CommandParamKind::U8 => r#"{"type":"u8"}"#.into(),
        CommandParamKind::U16 => r#"{"type":"u16"}"#.into(),
        CommandParamKind::U32 => r#"{"type":"u32"}"#.into(),
        CommandParamKind::U64 => r#"{"type":"u64"}"#.into(),
        CommandParamKind::U128 => r#"{"type":"u128"}"#.into(),
        CommandParamKind::I8 => r#"{"type":"i8"}"#.into(),
        CommandParamKind::I16 => r#"{"type":"i16"}"#.into(),
        CommandParamKind::I32 => r#"{"type":"i32"}"#.into(),
        CommandParamKind::I64 => r#"{"type":"i64"}"#.into(),
        CommandParamKind::I128 => r#"{"type":"i128"}"#.into(),
        CommandParamKind::F32 => r#"{"type":"f32"}"#.into(),
        CommandParamKind::F64 => r#"{"type":"f64"}"#.into(),
        CommandParamKind::Str => r#"{"type":"str"}"#.into(),
        CommandParamKind::Enum { type_name, variants } => {
            let vs: Vec<String> = variants.iter().map(|v| format!("\"{}\"", v)).collect();
            format!(
                r#"{{"type":"enum","type_name":"{}","variants":[{}]}}"#,
                type_name,
                vs.join(",")
            )
        }
    }
}

fn commands_response_json(rid: u32, defs: &[CommandDef]) -> String {
    let cmds: Vec<String> = defs.iter().map(|d| {
        let params: Vec<String> = d.params.iter().map(|p| {
            format!(r#"{{"name":"{}","kind":{}}}"#, p.name, command_param_kind_json(&p.kind))
        }).collect();
        let rk = match d.return_kind {
            CommandReturnKind::Unit => "unit",
            CommandReturnKind::Result => "result",
        };
        let desc = match d.description {
            Some(s) => format!("\"{}\"", s),
            None => "null".into(),
        };
        format!(
            r#"{{"name":"{}","description":{},"params":[{}],"return_kind":"{}"}}"#,
            d.name, desc, params.join(","), rk
        )
    }).collect();
    format!(
        r#"{{"type":"CommandsResponse","request_id":{},"commands":[{}]}}"#,
        rid, cmds.join(",")
    )
}

fn command_result_json(rid: u32, output: &CommandOutput, duration_ms: u32) -> String {
    let output_json = match output {
        CommandOutput::Unit => r#"{"ok":true}"#.into(),
        CommandOutput::Error(msg) => format!(r#"{{"ok":false,"error":"{}"}}"#, json_escape(msg)),
    };
    format!(
        r#"{{"type":"CommandResult","request_id":{},"output":{},"duration_ms":{}}}"#,
        rid, output_json, duration_ms
    )
}

fn set_value_ack(rid: u32, path: &str) -> String {
    format!(
        r#"{{"type":"SetValueAck","request_id":{},"path":"{}"}}"#,
        rid, path
    )
}

fn set_value_error(rid: u32, code: &str, path: &str) -> String {
    format!(
        r#"{{"type":"Error","request_id":{},"code":"{}","message":"{}"}}"#,
        rid, code, path
    )
}

fn parse_set_value(value_json: &str) -> Option<CommandArg> {
    let kind = json_str_field(value_json, "kind")?;
    match kind {
        "bool" => Some(CommandArg::Bool(json_bool_field(value_json, "value"))),
        "u8" => json_u32_field(value_json, "value").map(|v| CommandArg::U8(v as u8)),
        "u16" => json_u32_field(value_json, "value").map(|v| CommandArg::U16(v as u16)),
        "u32" => json_u32_field(value_json, "value").map(CommandArg::U32),
        "u64" => json_u32_field(value_json, "value").map(|v| CommandArg::U64(v as u64)),
        "i8" => json_u32_field(value_json, "value").map(|v| CommandArg::I8(v as i8)),
        "i16" => json_u32_field(value_json, "value").map(|v| CommandArg::I16(v as i16)),
        "i32" => json_u32_field(value_json, "value").map(|v| CommandArg::I32(v as i32)),
        "i64" => json_u32_field(value_json, "value").map(|v| CommandArg::I64(v as i64)),
        "f32" => json_u32_field(value_json, "value").map(|v| CommandArg::F32(v as f32)),
        "f64" => json_u32_field(value_json, "value").map(|v| CommandArg::F64(v as f64)),
        "str" | "string" => json_str_field(value_json, "value").map(|s| CommandArg::Str(s.into())),
        "enum" => json_str_field(value_json, "value").map(|s| CommandArg::Enum(s.into())),
        _ => None,
    }
}

fn debug_value_to_f64(v: DebugValue<'_>) -> Option<f64> {
    match v {
        DebugValue::U8(n) => Some(n as f64),
        DebugValue::U16(n) => Some(n as f64),
        DebugValue::U32(n) => Some(n as f64),
        DebugValue::U64(n) => Some(n as f64),
        DebugValue::U128(n) => Some(n as f64),
        DebugValue::I8(n) => Some(n as f64),
        DebugValue::I16(n) => Some(n as f64),
        DebugValue::I32(n) => Some(n as f64),
        DebugValue::I64(n) => Some(n as f64),
        DebugValue::I128(n) => Some(n as f64),
        DebugValue::F32(n) => Some(n as f64),
        DebugValue::F64(n) => Some(n),
        _ => None,
    }
}

fn parse_string_array(json: &str) -> Vec<String> {
    let mut result = Vec::new();
    let json = json.trim();
    if !json.starts_with('[') { return result; }
    let mut rest = &json[1..];
    loop {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_ascii_whitespace());
        if rest.is_empty() || rest.starts_with(']') { break; }
        if !rest.starts_with('"') { break; }
        rest = &rest[1..];
        if let Some(end) = rest.find('"') {
            result.push(rest[..end].into());
            rest = &rest[end + 1..];
        } else { break; }
    }
    result
}

fn metric_batch_json(timestamp_ms: u64, interval_ms: u32, samples: &[(String, f64)]) -> String {
    let sample_jsons: Vec<String> = samples
        .iter()
        .map(|(path, value)| format!(r#"{{"path":"{}","value":{}}}"#, path, value))
        .collect();
    format!(
        r#"{{"type":"MetricBatch","timestamp_ms":{},"interval_ms":{},"samples":[{}]}}"#,
        timestamp_ms,
        interval_ms,
        sample_jsons.join(",")
    )
}

fn subscribe_metrics_ack_json(rid: u32, effective_interval_ms: u32, active_paths: &[String]) -> String {
    let path_jsons: Vec<String> = active_paths.iter().map(|p| format!("\"{}\"", p)).collect();
    format!(
        r#"{{"type":"SubscribeMetricsAck","request_id":{},"effective_interval_ms":{},"active_paths":[{}]}}"#,
        rid,
        effective_interval_ms,
        path_jsons.join(",")
    )
}

fn unsubscribe_metrics_ack_json(rid: u32, active_paths: &[String]) -> String {
    let path_jsons: Vec<String> = active_paths.iter().map(|p| format!("\"{}\"", p)).collect();
    format!(
        r#"{{"type":"UnsubscribeMetricsAck","request_id":{},"active_paths":[{}]}}"#,
        rid,
        path_jsons.join(",")
    )
}

fn find_matching_brace(s: &str) -> usize {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if escape { escape = false; continue; }
        if c == '\\' && in_str { escape = true; continue; }
        if c == '"' { in_str = !in_str; continue; }
        if in_str { continue; }
        if c == '{' { depth += 1; }
        else if c == '}' {
            depth -= 1;
            if depth == 0 { return i; }
        }
    }
    s.len().saturating_sub(1)
}

fn parse_command_args(args_json: &str) -> Vec<CommandArg> {
    // args_json: [{"kind":"u32","value":42},{"kind":"bool","value":true},...]
    let mut result = Vec::new();
    let mut rest = args_json.trim();
    if !rest.starts_with('[') { return result; }
    rest = &rest[1..];
    loop {
        rest = rest.trim_start_matches(|c: char| c == ',' || c.is_ascii_whitespace());
        if rest.is_empty() || rest.starts_with(']') { break; }
        if !rest.starts_with('{') { break; }
        let end = find_matching_brace(rest);
        let obj = &rest[..=end];
        rest = &rest[end + 1..];
        let kind = json_str_field(obj, "kind").unwrap_or("");
        let arg = match kind {
            "bool" => Some(CommandArg::Bool(json_bool_field(obj, "value"))),
            "u8" => json_u32_field(obj, "value").map(|v| CommandArg::U8(v as u8)),
            "u16" => json_u32_field(obj, "value").map(|v| CommandArg::U16(v as u16)),
            "u32" => json_u32_field(obj, "value").map(CommandArg::U32),
            "u64" => json_u32_field(obj, "value").map(|v| CommandArg::U64(v as u64)),
            "i8" => json_u32_field(obj, "value").map(|v| CommandArg::I8(v as i8)),
            "i16" => json_u32_field(obj, "value").map(|v| CommandArg::I16(v as i16)),
            "i32" => json_u32_field(obj, "value").map(|v| CommandArg::I32(v as i32)),
            "i64" => json_u32_field(obj, "value").map(|v| CommandArg::I64(v as i64)),
            "f32" => json_u32_field(obj, "value").map(|v| CommandArg::F32(v as f32)),
            "f64" => json_u32_field(obj, "value").map(|v| CommandArg::F64(v as f64)),
            "str" | "string" => json_str_field(obj, "value").map(|s| CommandArg::Str(s.into())),
            "enum" => json_str_field(obj, "value").map(|s| CommandArg::Enum(s.into())),
            _ => None,
        };
        if let Some(a) = arg { result.push(a); }
    }
    result
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

/// Announce this device on the LAN via mDNS/DNS-SD (_embedded-inspect._tcp.local).
///
/// Sends gratuitous multicast announcements every 30 s and responds to
/// incoming A / PTR queries.  On macOS (Bonjour) this makes the device
/// reachable at http://<hostname>.local:<PORT>/ without knowing its IP.
///
/// If the WiFi driver or AP does not forward multicast to the station,
/// incoming queries will never arrive; the announcements alone still
/// populate the Bonjour cache on macOS for ~1200 s per RFC 6762.
#[embassy_executor::task]
async fn mdns_task(stack: Stack<'static>) {
    // Wait until we have an IP before advertising.
    stack.wait_config_up().await;
    let cfg = match stack.config_v4() {
        Some(c) => c,
        None => return,
    };
    let ip = cfg.address.address();
    let oct = ip.octets();

    let hostname = format!("{}-{:02x}{:02x}", slugify(DEVICE_NAME), oct[2], oct[3]);
    let instance = String::from(DEVICE_NAME);

    esp_println::println!("[mdns] hostname={}.local  instance={}", hostname, instance);

    // --- UDP socket setup ---
    let mut rx_meta  = [PacketMetadata::EMPTY; 4];
    let mut rx_buf   = [0u8; 1500];
    let mut tx_meta  = [PacketMetadata::EMPTY; 4];
    let mut tx_buf   = [0u8; 1500];
    let mut socket   = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);

    // Best-effort multicast group join (requires embassy-net/multicast feature
    // and AP forwarding multicast to the associated station).
    if let Err(e) = stack.join_multicast_group(MDNS_IP) {
        esp_println::println!("[mdns] join_multicast_group failed: {:?}", e);
        // Continue anyway — announcements will still work.
    }

    if socket.bind(5353).is_err() {
        esp_println::println!("[mdns] failed to bind UDP 5353");
        return;
    }

    let mdns_ep = IpEndpoint::new(MDNS_IP, 5353);
    let announcement = build_mdns_announcement(&hostname, &instance, oct);

    // Send initial announcement immediately.
    let _ = socket.send_to(&announcement, mdns_ep).await;
    ilog!(
        LogLevel::Info, "mdns",
        "advertised {} at {}.{}.{}.{}:{} as _embedded-inspect._tcp.local",
        hostname, oct[0], oct[1], oct[2], oct[3], PORT
    );

    let mut next_announce = Instant::now() + Duration::from_secs(30);
    let mut recv_pkt = [0u8; 512];

    loop {
        let now = Instant::now();
        let remaining = if now >= next_announce {
            Duration::from_millis(1)
        } else {
            next_announce - now
        };

        let timed_out = match with_timeout(remaining, socket.recv_from(&mut recv_pkt)).await {
            Err(_) => true, // timeout = time to announce
            Ok(Ok((n, _from))) => {
                // Try to answer the query.
                if let Some(resp) = handle_mdns_query(&recv_pkt[..n], &hostname, &instance, oct) {
                    let _ = socket.send_to(&resp, mdns_ep).await;
                }
                false
            }
            Ok(Err(_)) => break, // socket error — give up
        };

        if timed_out || Instant::now() >= next_announce {
            let pkt = build_mdns_announcement(&hostname, &instance, oct);
            let _ = socket.send_to(&pkt, mdns_ep).await;
            next_announce = Instant::now() + Duration::from_secs(30);
        }
    }
}

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

    // Metric subscription state — per-connection, owned by this WS session.
    let mut subscribed_metric_paths: Vec<String> = Vec::new();
    let mut metric_interval_ms: u32 = 0;
    let mut last_metric: Option<Instant> = None;

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

        // Push MetricBatch when subscriptions are active and interval has elapsed.
        if !subscribed_metric_paths.is_empty() && metric_interval_ms > 0 {
            let should_sample = match last_metric {
                None => true,
                Some(t) => Instant::now() - t >= Duration::from_millis(metric_interval_ms as u64),
            };
            if should_sample {
                let ts = Instant::now().as_millis();
                let snap: AppState = state.lock(|cell| cell.borrow().clone());
                let samples: Vec<(String, f64)> = subscribed_metric_paths
                    .iter()
                    .filter_map(|path| {
                        snap.get_field_path(path)
                            .and_then(debug_value_to_f64)
                            .map(|v| (path.clone(), v))
                    })
                    .collect();
                if !samples.is_empty() {
                    let json = metric_batch_json(ts, metric_interval_ms, &samples);
                    let n = ws
                        .write(WebSocketSendMessageType::Text, true, json.as_bytes(), &mut tx_buf)
                        .unwrap_or(0);
                    if n > 0 && write_all(sock, &tx_buf[..n]).await.is_err() {
                        break 'outer;
                    }
                }
                last_metric = Some(Instant::now());
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
        let remaining_metric = if !subscribed_metric_paths.is_empty() && metric_interval_ms > 0 {
            match last_metric {
                None => Duration::from_millis(1),
                Some(t) => {
                    let e = Instant::now() - t;
                    let interval = Duration::from_millis(metric_interval_ms as u64);
                    if e >= interval { Duration::from_millis(10) } else { interval - e }
                }
            }
        } else {
            Duration::from_secs(3600)
        };
        let remaining = remaining.min(remaining_metric);

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
                                    InMsg::InvokeCommand { request_id, name, args_json } => {
                                        ilog!(
                                            LogLevel::Debug,
                                            "inspect",
                                            "InvokeCommand '{}' request_id={}",
                                            name,
                                            request_id
                                        );
                                        let args = parse_command_args(args_json);
                                        CMD_CHANNEL.send(CommandRequest {
                                            request_id,
                                            name: name.into(),
                                            args,
                                        }).await;
                                        let reply = match with_timeout(
                                            Duration::from_secs(5),
                                            CMD_RESP.wait(),
                                        ).await {
                                            Ok(resp) => command_result_json(
                                                resp.request_id,
                                                &resp.output,
                                                resp.duration_ms,
                                            ),
                                            Err(_) => command_result_json(
                                                request_id,
                                                &CommandOutput::Error("timeout: device busy".into()),
                                                0,
                                            ),
                                        };
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
                                        break 'inner;
                                    }
                                    InMsg::SetValue { request_id, path, value_json } => {
                                        ilog!(
                                            LogLevel::Debug,
                                            "inspect",
                                            "SetValue '{}' request_id={}",
                                            path,
                                            request_id
                                        );
                                        let reply = match parse_set_value(value_json) {
                                            None => set_value_error(request_id, "MalformedRequest", path),
                                            Some(value) => {
                                                SET_CHANNEL.send(SetValueRequest {
                                                    request_id,
                                                    path: path.into(),
                                                    value,
                                                }).await;
                                                match with_timeout(
                                                    Duration::from_secs(5),
                                                    SET_RESP.wait(),
                                                ).await {
                                                    Ok((_, SetValueResult::Ok)) => set_value_ack(request_id, path),
                                                    Ok((_, SetValueResult::ReadOnly)) => set_value_error(request_id, "ReadOnly", path),
                                                    Ok((_, SetValueResult::TypeMismatch)) => set_value_error(request_id, "TypeMismatch", path),
                                                    Ok((_, SetValueResult::OutOfBounds)) => set_value_error(request_id, "OutOfBounds", path),
                                                    Ok((_, SetValueResult::UnknownField)) => set_value_error(request_id, "UnknownPath", path),
                                                    Ok((_, SetValueResult::UnknownVariant)) => set_value_error(request_id, "UnknownVariant", path),
                                                    Err(_) => set_value_error(request_id, "Timeout", path),
                                                }
                                            }
                                        };
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
                                        break 'inner;
                                    }
                                    InMsg::SubscribeMetrics { request_id, paths_json, interval_ms } => {
                                        let paths = parse_string_array(paths_json);
                                        let effective_interval = interval_ms.max(100);
                                        for p in &paths {
                                            if !subscribed_metric_paths.contains(p) {
                                                subscribed_metric_paths.push(p.clone());
                                            }
                                        }
                                        metric_interval_ms = effective_interval;
                                        last_metric = None; // trigger immediate first batch
                                        let reply = subscribe_metrics_ack_json(
                                            request_id,
                                            effective_interval,
                                            &subscribed_metric_paths,
                                        );
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
                                    InMsg::UnsubscribeMetrics { request_id, paths_json } => {
                                        let paths = parse_string_array(paths_json);
                                        if paths.is_empty() {
                                            subscribed_metric_paths.clear();
                                            metric_interval_ms = 0;
                                            last_metric = None;
                                        } else {
                                            subscribed_metric_paths.retain(|p| !paths.contains(p));
                                        }
                                        let reply = unsubscribe_metrics_ack_json(
                                            request_id,
                                            &subscribed_metric_paths,
                                        );
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
            hello_ack(request_id, state)
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
        InMsg::GetCommands { request_id } => {
            commands_response_json(request_id, AppState::command_defs())
        }
        InMsg::GetScreenshot { .. }
        | InMsg::InvokeCommand { .. }
        | InMsg::SetValue { .. }
        | InMsg::SubscribeMetrics { .. }
        | InMsg::UnsubscribeMetrics { .. }
        | InMsg::Unknown => {
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
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        0x1234_5678_u64,
    );

    spawner.spawn(net_task(runner).expect("net_task"));
    spawner.spawn(connection(controller).expect("connection"));
    spawner.spawn(debug_server(stack, state, fb_state).expect("debug_server"));
    spawner.spawn(mdns_task(stack).expect("mdns_task"));

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

        // Drain command channel — dispatch to AppState and signal response.
        while let Ok(req) = CMD_CHANNEL.try_receive() {
            let start = Instant::now();
            let output = state.lock(|cell| {
                cell.borrow_mut()
                    .dispatch_command(&req.name, &req.args)
                    .unwrap_or_else(|e| CommandOutput::Error(format!("{:?}", e)))
            });
            let duration_ms = start.elapsed().as_millis() as u32;
            CMD_RESP.signal(CommandResponse {
                request_id: req.request_id,
                output,
                duration_ms,
            });
        }

        // Drain set-value channel — apply writable field mutations via DebugSetValue.
        while let Ok(req) = SET_CHANNEL.try_receive() {
            let result = state.lock(|cell| {
                cell.borrow_mut().set_field(&req.path, req.value)
            });
            SET_RESP.signal((req.request_id, result));
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
