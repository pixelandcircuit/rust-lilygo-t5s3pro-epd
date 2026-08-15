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

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources, tcp::TcpSocket};
use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{Config, ControllerConfig, Interface, WifiController, sta::StationConfig};
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

const SSID:     &str = match option_env!("WIFI_SSID") { Some(s) => s, None => "SSID" };
const PASSWORD: &str = match option_env!("WIFI_PASS") { Some(s) => s, None => "PASSWORD" };
const PORT:     u16  = 3000;

const INDEX_HTML: &[u8] = include_bytes!("assets/inspect_index.html");

// ── AppState ──────────────────────────────────────────────────────────────────

#[derive(DebugInspect, Default, Clone)]
struct NetworkState {
    #[inspect(read_only)] connected: bool,
    #[inspect(read_only)] ip_a:      u8,
    #[inspect(read_only)] ip_b:      u8,
    #[inspect(read_only)] ip_c:      u8,
    #[inspect(read_only)] ip_d:      u8,
    #[inspect(read_only)] rssi:      i8,
}

#[derive(DebugInspect, Default, Clone)]
struct DisplayInfo {
    #[inspect(read_only)] refresh_count: u32,
    #[inspect(read_only)] power_on:      bool,
}

#[derive(DebugInspect, Default, Clone)]
struct SystemInfo {
    #[inspect(read_only)] uptime_secs: u32,
    #[inspect(read_only)] free_heap:   u32,
}

#[derive(DebugInspect, Default, Clone)]
struct ContentState {
    #[inspect(read_only)] current_page: u32,
    #[inspect(read_only)] touch_x:      u16,
    #[inspect(read_only)] touch_y:      u16,
}

#[derive(DebugInspect, Default, Clone)]
struct AppState {
    #[inspect(read_only)] network: NetworkState,
    #[inspect(read_only)] display: DisplayInfo,
    #[inspect(read_only)] system:  SystemInfo,
    #[inspect(read_only)] content: ContentState,
}

type SharedState = Mutex<CriticalSectionRawMutex, RefCell<AppState>>;
static STATE: StaticCell<SharedState> = StaticCell::new();

// ── I/O helper ────────────────────────────────────────────────────────────────

// embassy-net's TcpSocket::write may return fewer bytes than requested.
async fn write_all(sock: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        match sock.write(data).await {
            Ok(0)  => return Err(()),
            Ok(n)  => data = &data[n..],
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

// ── JSON schema tree ──────────────────────────────────────────────────────────

enum SchemaNode {
    Struct    { type_name: String, fields: Vec<FieldNode> },
    Enum      { type_name: String, variants: Vec<String> },
    Primitive { primitive: String },
}

struct FieldNode {
    name:      String,
    read_only: bool,
    schema:    SchemaNode,
}

fn build_schema(inspect: &dyn Inspect) -> SchemaNode {
    match inspect.type_schema() {
        TypeSchema::Struct(s) => {
            let fields = s.fields.iter().map(|f| {
                let schema = if f.kind == ValueKind::Object {
                    inspect.get_field(f.name)
                        .and_then(|v| {
                            if let DebugValue::Object(sub) = v { Some(build_schema(sub)) }
                            else { None }
                        })
                        .unwrap_or(SchemaNode::Primitive { primitive: "object".into() })
                } else {
                    SchemaNode::Primitive {
                        primitive: format!("{:?}", f.kind).to_lowercase(),
                    }
                };
                FieldNode { name: f.name.into(), read_only: f.read_only, schema }
            }).collect();
            SchemaNode::Struct { type_name: s.type_name.into(), fields }
        }
        TypeSchema::Enum(e) => SchemaNode::Enum {
            type_name: e.type_name.into(),
            variants:  e.variants.iter().map(|v| String::from(*v)).collect(),
        },
    }
}

fn schema_to_json(node: &SchemaNode) -> String {
    match node {
        SchemaNode::Struct { type_name, fields } => {
            let field_jsons: Vec<String> = fields.iter().map(|f| {
                format!(r#"{{"name":"{}","read_only":{},"schema":{}}}"#,
                    f.name, f.read_only, schema_to_json(&f.schema))
            }).collect();
            format!(r#"{{"kind":"Struct","type_name":"{}","fields":[{}]}}"#,
                type_name, field_jsons.join(","))
        }
        SchemaNode::Enum { type_name, variants } => {
            let var_jsons: Vec<String> = variants.iter()
                .map(|v| format!("\"{}\"", v)).collect();
            format!(r#"{{"kind":"Enum","type_name":"{}","variants":[{}]}}"#,
                type_name, var_jsons.join(","))
        }
        SchemaNode::Primitive { primitive } => {
            format!(r#"{{"kind":"Primitive","primitive":"{}"}}"#, primitive)
        }
    }
}

fn debug_value_to_json(v: DebugValue<'_>) -> String {
    match v {
        DebugValue::Bool(b)   => format!("{}", b),
        DebugValue::U8(n)     => format!("{}", n),
        DebugValue::U16(n)    => format!("{}", n),
        DebugValue::U32(n)    => format!("{}", n),
        DebugValue::U64(n)    => format!("{}", n),
        DebugValue::U128(n)   => format!("\"{}\"", n),
        DebugValue::I8(n)     => format!("{}", n),
        DebugValue::I16(n)    => format!("{}", n),
        DebugValue::I32(n)    => format!("{}", n),
        DebugValue::I64(n)    => format!("{}", n),
        DebugValue::I128(n)   => format!("\"{}\"", n),
        DebugValue::F32(n)    => format!("{}", n),
        DebugValue::F64(n)    => format!("{}", n),
        DebugValue::Str(s)    => format!("\"{}\"", s),
        DebugValue::Object(o) => match o.type_schema() {
            TypeSchema::Enum(_)   =>
                format!("{{\"variant\":\"{}\"}}",
                    o.active_variant().unwrap_or("unknown")),
            TypeSchema::Struct(s) =>
                format!("{{\"object\":\"{}\"}}", s.type_name),
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
    let start  = json.find(needle.as_str())? + needle.len();
    let len    = json[start..].find('"')?;
    Some(&json[start..start + len])
}

fn json_u32_field(json: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{}\":", key);
    let rest   = &json[json.find(needle.as_str())? + needle.len()..];
    let end    = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ── Protocol messages ─────────────────────────────────────────────────────────

enum InMsg<'a> {
    Hello     { request_id: u32 },
    GetSchema { request_id: u32 },
    GetValue  { request_id: u32, path: &'a str },
    Unknown,
}

fn parse_msg(json: &str) -> InMsg<'_> {
    let request_id = json_u32_field(json, "request_id").unwrap_or(0);
    match json_str_field(json, "type") {
        Some("Hello")     => InMsg::Hello { request_id },
        Some("GetSchema") => InMsg::GetSchema { request_id },
        Some("GetValue")  => InMsg::GetValue {
            request_id,
            path: json_str_field(json, "path").unwrap_or(""),
        },
        _ => InMsg::Unknown,
    }
}

fn hello_ack(rid: u32) -> String {
    format!(r#"{{"type":"HelloAck","request_id":{},"version":1,"server_name":"inspect-esp32"}}"#, rid)
}
fn schema_resp(rid: u32, schema: &str) -> String {
    format!(r#"{{"type":"SchemaResponse","request_id":{},"schema":{}}}"#, rid, schema)
}
fn value_resp(rid: u32, path: &str, value: &str) -> String {
    format!(r#"{{"type":"ResponseValue","request_id":{},"path":"{}","value":{}}}"#, rid, path, value)
}
fn error_resp(rid: u32, code: &str, msg: &str) -> String {
    format!(r#"{{"type":"Error","request_id":{},"code":"{}","message":"{}"}}"#, rid, code, msg)
}
fn changed_resp(path: &str, value: &str, seq: u32) -> String {
    format!(r#"{{"type":"ValueChanged","path":"{}","value":{},"sequence":{}}}"#, path, value, seq)
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
async fn debug_server(stack: Stack<'static>, state: &'static SharedState) {
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

        esp_println::println!("[inspect] client connected");
        handle_connection(&mut socket, state).await;

        socket.close();
        socket.flush().await.ok();
        socket.abort();
        esp_println::println!("[inspect] client disconnected");
    }
}

// ── HTTP + WebSocket connection handler ───────────────────────────────────────

async fn handle_connection(socket: &mut TcpSocket<'_>, state: &'static SharedState) {
    let mut http_buf = vec![0u8; 1536];
    let mut http_len = 0usize;

    // Read until end-of-headers marker.
    loop {
        match with_timeout(Duration::from_secs(10), socket.read(&mut http_buf[http_len..])).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
            Ok(Ok(n)) => {
                http_len += n;
                if http_buf[..http_len].windows(4).any(|w| w == b"\r\n\r\n") { break; }
                if http_len >= http_buf.len() { return; }
            }
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 24];
    let mut request = httparse::Request::new(&mut headers);
    if request.parse(&http_buf[..http_len]).is_err() { return; }

    match embedded_websocket::read_http_header(
        request.headers.iter().map(|h| (h.name, h.value))
    ) {
        Ok(Some(ctx)) => {
            let mut ws = WebSocketServer::new_server();
            let mut resp_buf = vec![0u8; 512];
            let n = match ws.server_accept(&ctx.sec_websocket_key, None, &mut resp_buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            write_all(socket, &resp_buf[..n]).await.ok();
            esp_println::println!("[inspect] WebSocket upgrade OK");
            run_ws_session(&mut ws, socket, state).await;
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
    ws:    &mut WebSocketServer,
    sock:  &mut TcpSocket<'_>,
    state: &'static SharedState,
) {
    // Schema is purely structural ('static field metadata) — build from default
    // values so we never hold the critical section during recursive allocation.
    let schema = build_schema(&AppState::default());
    let schema_json = schema_to_json(&schema);

    let mut leaf_paths: Vec<String> = Vec::new();
    collect_leaf_paths(&schema, "", &mut leaf_paths);
    let mut snapshot: Vec<String> = leaf_paths.iter().map(|_| String::new()).collect();
    let mut seq: u32 = 0;

    let mut rx_buf   = vec![0u8; 2048];
    let mut pl_buf   = vec![0u8; 1536];
    let mut tx_buf   = vec![0u8; 2048];
    let mut buf_used = 0usize;

    let mut last_event = Instant::now();

    'outer: loop {
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
                        let n = ws.write(WebSocketSendMessageType::Text, true, msg.as_bytes(), &mut tx_buf)
                            .unwrap_or(0);
                        if n > 0 && write_all(sock, &tx_buf[..n]).await.is_err() { break 'outer; }
                    }
                }
            }
            last_event = Instant::now();
        }

        let elapsed   = Instant::now() - last_event;
        let remaining = if elapsed < Duration::from_secs(2) {
            Duration::from_secs(2) - elapsed
        } else {
            Duration::from_millis(20)
        };

        match with_timeout(remaining, sock.read(&mut rx_buf[buf_used..])).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Err(_timeout)          => continue,
            Ok(Ok(n))              => buf_used += n,
        }

        loop {
            if buf_used == 0 { break; }
            match ws.read(&rx_buf[..buf_used], &mut pl_buf) {
                Err(_)               => break 'outer,
                Ok(r) if r.len_from == 0 => break,
                Ok(r) => {
                    let payload = &pl_buf[..r.len_to];
                    let reply: Option<String> = match r.message_type {
                        WebSocketReceiveMessageType::Text => {
                            match core::str::from_utf8(payload) {
                                Ok(json) => Some(handle_msg(json, state, &schema_json)),
                                Err(_)   => Some(error_resp(0, "BadEncoding", "non-UTF-8")),
                            }
                        }
                        WebSocketReceiveMessageType::CloseMustReply => {
                            let n = ws.close(WebSocketCloseStatusCode::NormalClosure, None, &mut tx_buf)
                                .unwrap_or(0);
                            write_all(sock, &tx_buf[..n]).await.ok();
                            break 'outer;
                        }
                        WebSocketReceiveMessageType::Ping => {
                            let n = ws.write(WebSocketSendMessageType::Pong, true, payload, &mut tx_buf)
                                .unwrap_or(0);
                            write_all(sock, &tx_buf[..n]).await.ok();
                            None
                        }
                        _ => None,
                    };

                    if let Some(text) = reply {
                        let n = ws.write(WebSocketSendMessageType::Text, true, text.as_bytes(), &mut tx_buf)
                            .unwrap_or(0);
                        if write_all(sock, &tx_buf[..n]).await.is_err() { break 'outer; }
                    }

                    let consumed = r.len_from;
                    rx_buf.copy_within(consumed..buf_used, 0);
                    buf_used -= consumed;
                }
            }
        }
    }
}

fn handle_msg(json: &str, state: &'static SharedState, schema_json: &str) -> String {
    match parse_msg(json) {
        InMsg::Hello { request_id }     => hello_ack(request_id),
        InMsg::GetSchema { request_id } => schema_resp(request_id, schema_json),
        InMsg::GetValue { request_id, path } => {
            let result = state.lock(|cell| {
                let app = cell.borrow();
                app.get_field_path(path).map(debug_value_to_json)
            });
            match result {
                Some(v) => value_resp(request_id, path, &v),
                None    => error_resp(request_id, "UnknownPath",
                               &format!("no field at path: {}", path)),
            }
        }
        InMsg::Unknown => error_resp(0, "UnknownMessage", "unrecognised message type"),
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

    let state: &'static SharedState =
        STATE.init(Mutex::new(RefCell::new(AppState::default())));

    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    ).expect("display init");
    display.power_on();
    state.lock(|cell| cell.borrow_mut().display.power_on = true);

    let timg0  = TimerGroup::new(peripherals.TIMG0);
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
    ).expect("wifi init");

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        0x1234_5678_u64,
    );

    spawner.spawn(net_task(runner).expect("net_task"));
    spawner.spawn(connection(controller).expect("connection"));
    spawner.spawn(debug_server(stack, state).expect("debug_server"));

    esp_println::println!("[inspect] connecting to '{}'...", SSID);
    stack.wait_config_up().await;

    if let Some(cfg) = stack.config_v4() {
        let ip  = cfg.address.address();
        let oct = ip.octets();
        esp_println::println!(
            "[inspect] ready: http://{}.{}.{}.{}:{}/",
            oct[0], oct[1], oct[2], oct[3], PORT,
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

    let boot          = Instant::now();
    let mut refreshes = 0u32;
    let mut last_draw = 0u32;

    loop {
        let uptime = (Instant::now() - boot).as_secs() as u32;

        state.lock(|cell| {
            let mut s = cell.borrow_mut();
            s.system.uptime_secs = uptime;
            s.network.connected  = stack.is_link_up();
        });

        if uptime == 0 || uptime - last_draw >= 30 {
            last_draw = uptime;
            // Pass 1: drive all pixels to white so old QR/text doesn't ghost.
            display.fill(0xF).unwrap();
            display.flush(DrawMode::WhiteOnBlack).unwrap();
            // Pass 2: render new content.
            render_status(&mut display, state);
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
        (s.network.connected, s.network.ip_a, s.network.ip_b,
         s.network.ip_c, s.network.ip_d, s.system.uptime_secs)
    });

    Text::with_alignment(
        "embedded-inspect demo",
        Point::new(480, 40), style, Alignment::Center,
    ).draw(display).unwrap();

    let addr = if connected {
        format!("http://{}.{}.{}.{}:{}/", ip_a, ip_b, ip_c, ip_d, PORT)
    } else {
        String::from("connecting to WiFi...")
    };
    Text::with_alignment(&addr, Point::new(480, 80), style, Alignment::Center)
        .draw(display).unwrap();

    let up = format!("uptime: {}s", uptime);
    Text::with_alignment(&up, Point::new(480, 120), style, Alignment::Center)
        .draw(display).unwrap();

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
    let Ok(bits) = encode_auto(url.as_bytes(), ec) else { return };
    let version = bits.version();
    let Ok((data_cw, ec_cw)) = construct_codewords(&bits.into_bytes(), version, ec) else { return };

    let mut canvas = Canvas::new(version, ec);
    canvas.draw_all_functional_patterns();
    canvas.draw_data(&data_cw, &ec_cw);
    let canvas = canvas.apply_best_mask();
    let colors = canvas.into_colors();

    let width  = version.width() as i32;
    let quiet  = 4i32;
    let scale  = 8i32;
    let total  = (width + quiet * 2) * scale;
    let x0     = (960 - total) / 2;
    let y0     = 160i32;
    let dark   = PrimitiveStyle::with_fill(Gray4::BLACK);

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
