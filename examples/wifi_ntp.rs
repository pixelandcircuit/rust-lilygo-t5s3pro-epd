//! WiFi NTP time sync example.
//!
//! Connects to WiFi, queries time.google.com via NTP, sets the RTC, and shows
//! a console-style status log on the EPD display and serial output.
//!
//! Build with WIFI_SSID and WIFI_PASS set:
//!   export WIFI_SSID=MyNetwork WIFI_PASS=secret
//!   cargo run --example wifi_ntp

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};

use embassy_executor::Spawner;
use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    Runner, StackResources,
};
use embassy_time::{Duration, Timer};
use embedded_graphics::{
    geometry::Point,
    mono_font::{
        ascii::{FONT_7X13, FONT_9X18},
        MonoTextStyle,
    },
    pixelcolor::Gray4,
    prelude::*,
    text::{Alignment, Text},
    Drawable,
};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, rtc_cntl::Rtc,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{sta::StationConfig, Config, ControllerConfig, Interface, WifiController};
use static_cell::StaticCell;

use epaper::driver::display::{Display, DrawMode};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: StaticCell<$t> = StaticCell::new();
        STATIC_CELL.uninit().write(($val))
    }};
}

// Credentials from environment at build time; fall back to placeholders so
// the example still type-checks without env vars set.
const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "SSID",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "PASSWORD",
};

const NTP_ADDR: [u8; 4] = [216, 239, 35, 0]; // time.google.com
const NTP_UNIX_OFFSET: u64 = 2_208_988_800; // NTP epoch → Unix epoch (70 years in seconds)

// ── Console ──────────────────────────────────────────────────────────────────

struct Console<'d> {
    display: Display<'d>,
    lines: Vec<String>,
}

impl<'d> Console<'d> {
    fn new(display: Display<'d>) -> Self {
        Self {
            display,
            lines: Vec::new(),
        }
    }

    fn log(&mut self, msg: &str) {
        esp_println::println!("[ntp] {}", msg);
        self.lines.push(String::from(msg));
        if self.lines.len() > 30 {
            self.lines.remove(0);
        }
        self.render();
        self.display.flush(DrawMode::WhiteOnBlack).unwrap();
        self.render();
        self.display.flush(DrawMode::BlackOnWhite).unwrap();
    }

    fn render(&mut self) {
        self.display.fill(0xF).unwrap();

        Text::with_alignment(
            "=== WiFi NTP Time Sync ===",
            Point::new(480, 18),
            MonoTextStyle::new(&FONT_9X18, Gray4::BLACK),
            Alignment::Center,
        )
        .draw(&mut self.display)
        .unwrap();

        let style = MonoTextStyle::new(&FONT_7X13, Gray4::BLACK);
        for (i, line) in self.lines.iter().enumerate() {
            Text::with_alignment(
                line.as_str(),
                Point::new(8, 50 + i as i32 * 16),
                style,
                Alignment::Left,
            )
            .draw(&mut self.display)
            .unwrap();
        }
    }
}

// ── Embassy tasks ─────────────────────────────────────────────────────────────

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(_info) => {
                controller.wait_for_disconnect_async().await.ok();
                esp_println::println!("[ntp] WiFi disconnected; retrying...");
            }
            Err(e) => {
                esp_println::println!("[ntp] Connect failed: {:?}", e);
            }
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // PSRAM for display framebuffer; SRAM for WiFi stack
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // Display
    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");
    display.power_on();

    // RTC — used for NTP seed and to store the synced time
    let rtc = Rtc::new(peripherals.LPWR);

    let mut console = Console::new(display);
    console.log("Booted");

    // Start WiFi scheduler (must come before creating the WiFi controller)
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // WiFi init
    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(SSID)
            .with_password(PASSWORD.into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .expect("wifi init");

    // Embassy-net stack (uses RTC uptime as random seed — differs each boot)
    let seed = rtc.current_time_us();
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(net_task(runner).expect("net_task"));
    spawner.spawn(connection(controller).expect("connection"));

    console.log(&format!("Connecting to '{}'...", SSID));
    stack.wait_config_up().await;

    let ip_msg = match stack.config_v4() {
        Some(cfg) => format!("IP: {}", cfg.address),
        None => String::from("IP acquired"),
    };
    console.log(&ip_msg);

    // NTP query via raw 48-byte UDP packet to time.google.com:123
    console.log("Querying NTP (time.google.com:123)...");

    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 512];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_buf = [0u8; 256];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(12345).expect("bind");

    let ntp_endpoint = embassy_net::IpEndpoint::new(
        embassy_net::IpAddress::Ipv4(embassy_net::Ipv4Address::from_octets(NTP_ADDR)),
        123,
    );

    let mut pkt = [0u8; 48];
    pkt[0] = 0x1B; // LI=0, VN=3, Mode=3 (client)

    if let Err(e) = socket.send_to(&pkt, ntp_endpoint).await {
        console.log(&format!("ERROR: NTP send: {:?}", e));
        loop {
            Timer::after(Duration::from_secs(60)).await;
        }
    }

    let (n, _from) = match socket.recv_from(&mut pkt).await {
        Ok(r) => r,
        Err(e) => {
            console.log(&format!("ERROR: NTP recv: {:?}", e));
            loop {
                Timer::after(Duration::from_secs(60)).await;
            }
        }
    };

    if n < 48 {
        console.log(&format!("ERROR: NTP response too short ({} bytes)", n));
        loop {
            Timer::after(Duration::from_secs(60)).await;
        }
    }

    // Parse NTP transmit timestamp at bytes 40–47 (big-endian seconds + fraction)
    let ntp_secs = u32::from_be_bytes([pkt[40], pkt[41], pkt[42], pkt[43]]) as u64;
    let ntp_frac = u32::from_be_bytes([pkt[44], pkt[45], pkt[46], pkt[47]]) as u64;

    if ntp_secs <= NTP_UNIX_OFFSET {
        console.log("ERROR: NTP returned pre-Unix-epoch time (server error?)");
        loop {
            Timer::after(Duration::from_secs(60)).await;
        }
    }

    let unix_secs = ntp_secs - NTP_UNIX_OFFSET;
    let unix_us = unix_secs * 1_000_000 + ((ntp_frac * 1_000_000) >> 32);

    rtc.set_current_time_us(unix_us);

    let (hh, mm, ss) = secs_to_hms(unix_secs);
    console.log(&format!("RTC set! UTC: {:02}:{:02}:{:02}", hh, mm, ss));

    // Show live clock updating every 10 seconds
    loop {
        Timer::after(Duration::from_secs(10)).await;
        let now_secs = rtc.current_time_us() / 1_000_000;
        let (hh, mm, ss) = secs_to_hms(now_secs);
        console.log(&format!("Current UTC: {:02}:{:02}:{:02}", hh, mm, ss));
    }
}

fn secs_to_hms(unix_secs: u64) -> (u64, u64, u64) {
    let sod = unix_secs % 86400;
    (sod / 3600, (sod % 3600) / 60, sod % 60)
}
