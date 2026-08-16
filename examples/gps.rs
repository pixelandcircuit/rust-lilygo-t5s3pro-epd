//! GPS example — reads NMEA sentences from the onboard L76K / MIA-M10Q GPS
//! module and prints location fixes to serial.
//!
//! The GPS module shares the PCA9555 I/O expander power rail with the LoRa
//! module. Port-0 bit 0 of the expander (I2C 0x20) must be driven high to
//! power the GPS on. UART1 is then opened on the dedicated GPS pins:
//!   GPIO43 = BOARD_GPS_TXD (ESP32 → GPS)
//!   GPIO44 = BOARD_GPS_RXD (GPS  → ESP32)
//!
//! Both the L76K and MIA-M10Q default to 9600 baud NMEA output; no
//! chip-specific init commands are needed to start receiving fixes.
//!
//! Run: cargo run --example gps

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    i2c::master::{Config as I2cConfig, I2c},
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

const PCA9555_ADDR: u8 = 0x20;
const PCA_REG_CONFIG0: u8 = 0x06; // port-0 direction register  (0 = output)
const PCA_REG_OUTPUT0: u8 = 0x02; // port-0 output latch

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    // Enable GPS/LoRa power via PCA9555.
    // Port-0 bit 0 is the power rail; driving the whole port high matches
    // what the display driver does during its own init.
    let mut i2c = I2c::new(peripherals.I2C0, I2cConfig::default())
        .expect("I2C init")
        .with_sda(peripherals.GPIO39)
        .with_scl(peripherals.GPIO40);
    let _ = i2c.write(PCA9555_ADDR, &[PCA_REG_CONFIG0, 0x00]); // port-0: all outputs
    let _ = i2c.write(PCA9555_ADDR, &[PCA_REG_OUTPUT0, 0xFF]); // port-0: all high

    let delay = Delay::new();
    delay.delay_millis(500); // allow GPS module to power up and start NMEA output

    // UART1 at 9600 baud — factory default for both L76K and MIA-M10Q
    let uart_config = UartConfig::default().with_baudrate(9600);
    let mut uart = Uart::new(peripherals.UART1, uart_config)
        .expect("UART1 init")
        .with_rx(peripherals.GPIO44) // GPS TX  → ESP32 RX
        .with_tx(peripherals.GPIO43); // ESP32 TX → GPS RX

    println!("[gps] listening for NMEA — fix may take 30–60 s outdoors...");

    let mut line = [0u8; 128];
    let mut pos = 0usize;
    let mut byte = [0u8; 1];

    loop {
        if uart.read(&mut byte).is_err() {
            continue;
        }
        match byte[0] {
            b'\n' => {
                if let Ok(s) = core::str::from_utf8(&line[..pos]) {
                    // GGA carries time, position, and fix quality
                    if s.starts_with("$GNGGA") || s.starts_with("$GPGGA") {
                        print_gga(s);
                    }
                }
                pos = 0;
            }
            b'\r' => {} // ignore CR
            b => {
                if pos < line.len() - 1 {
                    line[pos] = b;
                    pos += 1;
                }
            }
        }
    }
}

/// Parse a GGA sentence and print location to serial.
///
/// Format: $GNGGA,HHMMSS.ss,DDMM.mmmm,N,DDDMM.mmmm,E,fix,sats,hdop,alt,M,...
/// fix: 0 = no fix  1 = GPS  2 = DGPS
fn print_gga(sentence: &str) {
    let mut f = sentence.splitn(15, ',');
    let _tag = f.next();
    let time = f.next().unwrap_or("");
    let lat = f.next().unwrap_or("");
    let ns = f.next().unwrap_or("");
    let lon = f.next().unwrap_or("");
    let ew = f.next().unwrap_or("");
    let fix = f.next().unwrap_or("0");
    let sats = f.next().unwrap_or("0");
    let hdop = f.next().unwrap_or("");
    let alt = f.next().unwrap_or("");

    if fix == "0" || lat.is_empty() {
        println!("[gps] no fix (sats tracked: {})", sats);
        return;
    }

    println!(
        "[gps] fix={} time={}  {}{} {}{}  sats={}  hdop={}  alt={}m",
        fix, time, lat, ns, lon, ew, sats, hdop, alt
    );

    if let (Some(lat_d), Some(lon_d)) = (nmea_to_decimal(lat, ns), nmea_to_decimal(lon, ew)) {
        println!("[gps]              {:.6}°  {:.6}°", lat_d, lon_d);
    }
}

/// Convert NMEA DDMM.mmmm / DDDMM.mmmm + hemisphere to decimal degrees.
/// Latitude is DDMM, longitude is DDDMM (3-digit degree prefix).
/// S and W are negative.
fn nmea_to_decimal(value: &str, dir: &str) -> Option<f64> {
    let v: f64 = value.parse().ok()?;
    let degrees = (v / 100.0) as u32 as f64; // truncate; safe because NMEA values are always positive
    let minutes = v - degrees * 100.0;
    let dd = degrees + minutes / 60.0;
    match dir {
        "S" | "W" => Some(-dd),
        _ => Some(dd),
    }
}
