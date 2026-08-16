#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use esp_backtrace as _;
use esp_hal::{delay::Delay, main};
use esp_println::println;

use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_9X18},
        MonoTextStyle,
    },
    pixelcolor::Gray4,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};

use epaper::driver::{Display, DrawMode};

esp_bootloader_esp_idf::esp_app_desc!();

// ── I2C addresses ─────────────────────────────────────────────────────────────
const BQ27220_ADDR: u8 = 0x55; // fuel gauge
const BQ25896_ADDR: u8 = 0x6B; // charger

// ── BQ27220 standard command registers (little-endian u16) ────────────────────
const FG_TEMPERATURE: u8 = 0x06; // 0.1 K
const FG_VOLTAGE: u8 = 0x08; // mV
const FG_CURRENT: u8 = 0x0C; // mA, signed
const FG_REMAINING_CAP: u8 = 0x10; // mAh
const FG_FULL_CHARGE_CAP: u8 = 0x12; // mAh
const FG_STATE_OF_CHARGE: u8 = 0x2C; // %
const FG_STATE_OF_HEALTH: u8 = 0x2E; // %

// ── BQ25896 registers ─────────────────────────────────────────────────────────
const CHG_STATUS_REG: u8 = 0x0B; // charge status & power-good
const CHG_VBAT_REG: u8 = 0x0E; // battery voltage ADC
const CHG_VSYS_REG: u8 = 0x0F; // system voltage ADC
const CHG_VBUS_REG: u8 = 0x11; // VBUS voltage ADC
const CHG_ICHG_REG: u8 = 0x12; // charge current ADC

// REG0B bit/field positions
const CHG_PG_BIT: u8 = 1 << 2; // bit 2 – USB power good
const CHG_STAT_MASK: u8 = 0x03; // bits [4:3] – charge state

// ── Data structs ──────────────────────────────────────────────────────────────

struct FuelGauge {
    voltage_mv: u16,
    current_ma: i16,
    soc: u16,
    health: u16,
    remaining_mah: u16,
    full_mah: u16,
    temp_c: f32,
}

struct Charger {
    power_good: bool,
    status_str: &'static str,
    vbus_mv: u16,
    vbat_mv: u16,
    vsys_mv: u16,
    ichg_ma: u16,
}

// ── Charge status decode ──────────────────────────────────────────────────────

fn charge_status_str(reg0b: u8) -> &'static str {
    match (reg0b >> 3) & CHG_STAT_MASK {
        0 => "Not charging",
        1 => "Pre-charge",
        2 => "Fast charging",
        3 => "Charge done",
        _ => "Unknown",
    }
}

// ── Sensor reads ──────────────────────────────────────────────────────────────

fn read_fuel_gauge(display: &mut Display) -> FuelGauge {
    let voltage_mv = display.i2c_read_u16(BQ27220_ADDR, FG_VOLTAGE);
    let current_ma = display.i2c_read_i16(BQ27220_ADDR, FG_CURRENT);
    let soc = display.i2c_read_u16(BQ27220_ADDR, FG_STATE_OF_CHARGE);
    let health = display.i2c_read_u16(BQ27220_ADDR, FG_STATE_OF_HEALTH);
    let remaining_mah = display.i2c_read_u16(BQ27220_ADDR, FG_REMAINING_CAP);
    let full_mah = display.i2c_read_u16(BQ27220_ADDR, FG_FULL_CHARGE_CAP);
    let raw_temp = display.i2c_read_u16(BQ27220_ADDR, FG_TEMPERATURE);
    // BQ27220 reports temperature in 0.1 K; convert to Celsius
    let temp_c = raw_temp as f32 / 10.0 - 273.15;

    println!(
        "FG: {}mV  {}mA  soc={}%  health={}%  rem={}mAh  full={}mAh  {:.1}C",
        voltage_mv, current_ma, soc, health, remaining_mah, full_mah, temp_c
    );

    FuelGauge {
        voltage_mv,
        current_ma,
        soc,
        health,
        remaining_mah,
        full_mah,
        temp_c,
    }
}

fn read_charger(display: &mut Display) -> Charger {
    let reg0b = display.i2c_read_u8(BQ25896_ADDR, CHG_STATUS_REG);
    let power_good = reg0b & CHG_PG_BIT != 0;
    let status_str = charge_status_str(reg0b);

    // VBUS: base 2600 mV + step 100 mV per LSB (valid only when USB present)
    let vbus_raw = display.i2c_read_u8(BQ25896_ADDR, CHG_VBUS_REG);
    let vbus_mv = if power_good {
        2600u16 + (vbus_raw & 0x7F) as u16 * 100
    } else {
        0
    };

    // VBAT / VSYS: base 2304 mV + step 20 mV per LSB
    let vbat_raw = display.i2c_read_u8(BQ25896_ADDR, CHG_VBAT_REG);
    let vbat_mv = 2304u16 + (vbat_raw & 0x7F) as u16 * 20;
    let vsys_raw = display.i2c_read_u8(BQ25896_ADDR, CHG_VSYS_REG);
    let vsys_mv = 2304u16 + (vsys_raw & 0x7F) as u16 * 20;

    // ICHG: step 50 mA per LSB
    let ichg_raw = display.i2c_read_u8(BQ25896_ADDR, CHG_ICHG_REG);
    let ichg_ma = (ichg_raw & 0x7F) as u16 * 50;

    println!(
        "CHG: pg={}  status={}  vbus={}mV  vbat={}mV  vsys={}mV  ichg={}mA",
        power_good, status_str, vbus_mv, vbat_mv, vsys_mv, ichg_ma
    );

    Charger {
        power_good,
        status_str,
        vbus_mv,
        vbat_mv,
        vsys_mv,
        ichg_ma,
    }
}

// ── Screen render ─────────────────────────────────────────────────────────────

const W: i32 = Display::WIDTH as i32;
const H: i32 = Display::HEIGHT as i32;
const PAD: i32 = 24;
const MID: i32 = W / 2;
const COL2: i32 = MID + 20;

fn render(display: &mut Display, fg: &FuelGauge, ch: &Charger) {
    display.clear().unwrap();

    let large = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);
    let large_inv = MonoTextStyle::new(&FONT_10X20, Gray4::WHITE);
    let small = MonoTextStyle::new(&FONT_9X18, Gray4::BLACK);
    let fill = PrimitiveStyle::with_fill(Gray4::BLACK);
    let rule = PrimitiveStyle::with_stroke(Gray4::BLACK, 1);

    // ── Title bar ─────────────────────────────────────────────────────────────
    Rectangle::new(Point::zero(), Size::new(W as u32, 44))
        .into_styled(fill)
        .draw(display)
        .unwrap();
    Text::with_alignment(
        "Battery Status",
        Point::new(MID, 30),
        large_inv,
        Alignment::Center,
    )
    .draw(display)
    .unwrap();

    // Vertical column divider
    Line::new(Point::new(MID, 44), Point::new(MID, H - PAD))
        .into_styled(rule)
        .draw(display)
        .unwrap();

    // ── Left: BQ27220 fuel gauge ──────────────────────────────────────────────
    let lx = PAD;
    let mut y = 78i32;

    Text::with_alignment(
        "Fuel Gauge  (BQ27220)",
        Point::new(lx, y),
        small,
        Alignment::Left,
    )
    .draw(display)
    .unwrap();
    y += 14;
    Line::new(Point::new(lx, y), Point::new(MID - 20, y))
        .into_styled(rule)
        .draw(display)
        .unwrap();
    y += 28;

    let fg_rows: &[(&str, alloc::string::String)] = &[
        ("State of charge", format!("{}%", fg.soc)),
        ("Voltage", format!("{} mV", fg.voltage_mv)),
        ("Current", format!("{} mA", fg.current_ma)),
        (
            "Direction",
            format!(
                "{}",
                if fg.current_ma > 0 {
                    "Charging"
                } else if fg.current_ma < 0 {
                    "Discharging"
                } else {
                    "Idle"
                }
            ),
        ),
        ("Remaining", format!("{} mAh", fg.remaining_mah)),
        ("Full capacity", format!("{} mAh", fg.full_mah)),
        ("State of health", format!("{}%", fg.health)),
        ("Temperature", format!("{:.1} C", fg.temp_c)),
    ];
    for (label, value) in fg_rows {
        Text::with_alignment(label, Point::new(lx, y), small, Alignment::Left)
            .draw(display)
            .unwrap();
        Text::with_alignment(value, Point::new(MID - 20, y), large, Alignment::Right)
            .draw(display)
            .unwrap();
        y += 36;
    }

    // ── Right: BQ25896 charger ────────────────────────────────────────────────
    let rx = COL2;
    let mut y = 78i32;

    Text::with_alignment(
        "Charger  (BQ25896)",
        Point::new(rx, y),
        small,
        Alignment::Left,
    )
    .draw(display)
    .unwrap();
    y += 14;
    Line::new(Point::new(rx, y), Point::new(W - PAD, y))
        .into_styled(rule)
        .draw(display)
        .unwrap();
    y += 28;

    let ch_rows: &[(&str, alloc::string::String)] = &[
        (
            "USB power",
            format!(
                "{}",
                if ch.power_good {
                    "Present"
                } else {
                    "Not connected"
                }
            ),
        ),
        ("Charge status", format!("{}", ch.status_str)),
        ("VBUS", format!("{} mV", ch.vbus_mv)),
        ("Battery voltage", format!("{} mV", ch.vbat_mv)),
        ("System voltage", format!("{} mV", ch.vsys_mv)),
        ("Charge current", format!("{} mA", ch.ichg_ma)),
    ];
    for (label, value) in ch_rows {
        Text::with_alignment(label, Point::new(rx, y), small, Alignment::Left)
            .draw(display)
            .unwrap();
        Text::with_alignment(value, Point::new(W - PAD, y), large, Alignment::Right)
            .draw(display)
            .unwrap();
        y += 36;
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    let psram_config = esp_hal::psram::PsramConfig {
        mode: esp_hal::psram::PsramMode::OctalSpi,
        ..Default::default()
    };
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram, psram_config);

    let delay = Delay::new();

    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");

    delay.delay_millis(100);
    display.power_on();
    delay.delay_millis(10);

    println!("Battery status demo starting");

    loop {
        let fg = read_fuel_gauge(&mut display);
        let ch = read_charger(&mut display);

        render(&mut display, &fg, &ch);
        display.flush(DrawMode::BlackOnWhite).unwrap();

        println!("Screen updated — refreshing in 10 s");
        delay.delay_millis(10_000);
    }
}
