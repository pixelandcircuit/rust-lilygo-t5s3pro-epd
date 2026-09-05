//! Open WiFi access point, QR code, and tiny HTTP server.
//!
//! Connect to `epaper-device`, scan the displayed QR code, then open
//! `http://epaper-device.local/` in the phone's browser. The numeric address
//! `http://192.168.4.1/` remains available as a fallback.
//!
//! Run with: `cargo run --example ap_web_qr --features ap-web`
#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};
use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::{
    tcp::TcpSocket, IpListenEndpoint, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
};
use embassy_time::{Duration, Timer};
use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_7X13},
        MonoTextStyle,
    },
    pixelcolor::{Gray4, GrayColor},
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use embedded_io_async::Write;
use epaper::driver::{Display, DrawMode};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, rng::Rng,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{ap::AccessPointConfig, Config, ControllerConfig, Interface, WifiController};
use qrcode_core::{
    bits::encode_auto,
    canvas::Canvas,
    ec::construct_codewords,
    types::{Color, EcLevel},
};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = "epaper-device";
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
const HTTP_PORT: u16 = 80;
const HOSTNAME: &str = "epaper-device.local";
const MDNS_IP: embassy_net::IpAddress = embassy_net::IpAddress::v4(224, 0, 0, 251);
const INDEX_HTML: &[u8] = br#"<!doctype html><meta name=viewport content='width=device-width,initial-scale=1'><title>E-Paper Device</title><style>body{font:20px system-ui;max-width:36em;margin:3em auto;padding:0 1em;color:#222}code{background:#eee;padding:.2em .4em}</style><h1>Hello from the E-Paper</h1><p>This page is served directly by the LilyGO T5 E-Paper S3 Pro.</p><p>WiFi access point: <code>epaper-device</code></p><p>Device address: <code>192.168.4.1</code></p>"#;

macro_rules! mk_static {
    ($t:ty, $value:expr) => {{
        static CELL: StaticCell<$t> = StaticCell::new();
        CELL.uninit().write($value)
    }};
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn ap_task(mut controller: WifiController<'static>) {
    let config = Config::AccessPoint(AccessPointConfig::default().with_ssid(SSID));
    controller.set_config(&config).expect("AP config");
    esp_println::println!("AP started: {}", SSID);
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    use edge_dhcp::{
        io::{server::run, DEFAULT_SERVER_PORT},
        server::{Server, ServerOptions},
    };
    use edge_nal::UdpBind;
    use edge_nal_embassy::{Udp, UdpBuffers};
    let ip = GATEWAY;
    let mut buf = [0u8; 1500];
    let mut gateway = [Ipv4Addr::UNSPECIFIED];
    let buffers = UdpBuffers::<3, 1024, 1024, 10>::new();
    let udp = Udp::new(stack, &buffers);
    let mut socket = udp
        .bind(core::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DEFAULT_SERVER_PORT).into())
        .await
        .unwrap();
    let mut server = Server::<_, 64>::new_with_et(ip);
    run(
        &mut server,
        &ServerOptions::new(ip, Some(&mut gateway)),
        &mut socket,
        &mut buf,
    )
    .await
    .ok();
}

#[embassy_executor::task]
async fn http_task(stack: Stack<'static>) {
    loop {
        let mut rx = [0u8; 1024];
        let mut tx = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(5)));
        if socket
            .accept(IpListenEndpoint {
                addr: None,
                port: HTTP_PORT,
            })
            .await
            .is_ok()
        {
            let mut request = [0u8; 512];
            let _ = socket.read(&mut request).await;
            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", INDEX_HTML.len());
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(INDEX_HTML).await;
            let _ = socket.flush().await;
        }
        socket.close();
        socket.abort();
    }
}

fn dns_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn mdns_answer(query: &[u8], ip: Ipv4Addr) -> Option<Vec<u8>> {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return None;
    }
    let mut pos = 12;
    let mut name = String::new();
    while pos < query.len() {
        let len = query[pos] as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        if len > 63 || pos + len > query.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(core::str::from_utf8(&query[pos..pos + len]).ok()?);
        pos += len;
    }
    if !name.eq_ignore_ascii_case(HOSTNAME) || pos + 4 > query.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([query[pos], query[pos + 1]]);
    if qtype != 1 && qtype != 255 {
        return None;
    }
    let mut response = Vec::with_capacity(64);
    response.extend_from_slice(&query[..2]);
    response.extend_from_slice(&[0x84, 0x00, 0, 0, 0, 1, 0, 0, 0, 0]);
    dns_name(&mut response, HOSTNAME);
    response.extend_from_slice(&[0, 1, 0, 1]);
    response.extend_from_slice(&[0, 0, 0, 120, 0, 4]);
    response.extend_from_slice(&ip.octets());
    Some(response)
}

#[embassy_executor::task]
async fn mdns_task(stack: Stack<'static>) {
    stack.wait_config_up().await;
    let Some(config) = stack.config_v4() else {
        return;
    };
    let ip = config.address.address();
    let mut rx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 512];
    let mut tx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 4];
    let mut tx_buf = [0u8; 512];
    let mut socket = embassy_net::udp::UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buf,
        &mut tx_meta,
        &mut tx_buf,
    );
    let _ = stack.join_multicast_group(MDNS_IP);
    if socket.bind(5353).is_err() {
        return;
    }
    let endpoint = embassy_net::IpEndpoint::new(MDNS_IP, 5353);
    let mut packet = [0u8; 512];
    loop {
        if let Ok((len, _)) = socket.recv_from(&mut packet).await {
            if let Some(answer) = mdns_answer(&packet[..len], ip) {
                let _ = socket.send_to(&answer, endpoint).await;
            }
        }
    }
}

fn draw_qr(display: &mut Display, value: &str) {
    let Ok(bits) = encode_auto(value.as_bytes(), EcLevel::M) else {
        return;
    };
    let version = bits.version();
    let Ok((data, ecc)) = construct_codewords(&bits.into_bytes(), version, EcLevel::M) else {
        return;
    };
    let mut canvas = Canvas::new(version, EcLevel::M);
    canvas.draw_all_functional_patterns();
    canvas.draw_data(&data, &ecc);
    let colors = canvas.apply_best_mask().into_colors();
    let width = version.width() as i32;
    let scale = 6i32;
    let quiet = 4i32;
    let total = (width + quiet * 2) * scale;
    let x0 = (960 - total) / 2;
    let y0 = 70;
    for row in 0..width {
        for col in 0..width {
            if colors[(row * width + col) as usize] == Color::Dark {
                Rectangle::new(
                    Point::new(x0 + (col + quiet) * scale, y0 + (row + quiet) * scale),
                    Size::new(scale as u32, scale as u32),
                )
                .into_styled(PrimitiveStyle::with_fill(Gray4::BLACK))
                .draw(display)
                .ok();
            }
        }
    }
}

fn draw_screen(display: &mut Display) {
    display.fill(0xF).unwrap();
    let small = MonoTextStyle::new(&FONT_7X13, Gray4::BLACK);
    let large = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);
    Text::with_alignment(
        "WiFi: epaper-device",
        Point::new(480, 18),
        large,
        Alignment::Center,
    )
    .draw(display)
    .unwrap();
    Text::with_alignment(
        "Scan QR or connect manually, then type:",
        Point::new(480, 43),
        small,
        Alignment::Center,
    )
    .draw(display)
    .unwrap();
    Text::with_alignment(HOSTNAME, Point::new(480, 67), large, Alignment::Center)
        .draw(display)
        .unwrap();
    draw_qr(display, "WIFI:T:nopass;S:epaper-device;;");
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    esp_alloc::heap_allocator!(size: 72 * 1024);
    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");
    display.power_on();
    display.clear().unwrap();
    draw_screen(&mut display);
    display.flush(DrawMode::BlackOnWhite).unwrap();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    let radio =
        esp_radio::wifi::new(peripherals.WIFI, ControllerConfig::default()).expect("wifi init");
    let (controller, interfaces) = radio;
    let seed = Rng::new().random() as u64;
    let net_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(GATEWAY, 24),
        gateway: Some(GATEWAY),
        dns_servers: Default::default(),
    });
    let (stack, runner) = embassy_net::new(
        interfaces.access_point,
        net_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(ap_task(controller).unwrap());
    spawner.spawn(dhcp_task(stack).unwrap());
    spawner.spawn(http_task(stack).unwrap());
    spawner.spawn(mdns_task(stack).unwrap());
    esp_println::println!("Open WiFi AP '{}' and browse to http://{}/", SSID, HOSTNAME);
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
