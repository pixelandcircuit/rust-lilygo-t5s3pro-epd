#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Input, InputConfig, Pull},
    main,
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

// GPIOs not used by the display driver or other known peripherals.
// GPIO0       = BOOT button (skip — already known)
// GPIO4       = CKH, GPIO5-8,15-18 = data bus, GPIO11 = backlight
// GPIO39,40   = I2C, GPIO41,42,45,48 = STH/LEH/STV/CKV
// GPIO47      = BOARD_LORA_BUSY
// GPIO19,20   = native USB D-/D+ — DO NOT touch or the port dies
// GPIO33-37   = PSRAM (internal, do not reconfigure)
// Candidates: everything else that's plausibly a free user button.
macro_rules! poll_pins {
    ($delay:expr, $( ($name:literal, $pin:expr) ),+ $(,)?) => {
        loop {
            $( if $pin.is_low() { println!("LOW: {}", $name); } )+
            $delay.delay_millis(100);
        }
    }
}

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    let cfg = InputConfig::default().with_pull(Pull::Up);

    let p1 = Input::new(peripherals.GPIO1, cfg);
    let p2 = Input::new(peripherals.GPIO2, cfg);
    let p3 = Input::new(peripherals.GPIO3, cfg);
    let p9 = Input::new(peripherals.GPIO9, cfg);
    let p10 = Input::new(peripherals.GPIO10, cfg);
    let p12 = Input::new(peripherals.GPIO12, cfg);
    let p13 = Input::new(peripherals.GPIO13, cfg);
    let p14 = Input::new(peripherals.GPIO14, cfg);
    let p21 = Input::new(peripherals.GPIO21, cfg);
    let p38 = Input::new(peripherals.GPIO38, cfg);
    let p46 = Input::new(peripherals.GPIO46, cfg);

    let delay = Delay::new();

    println!("find_button: press the forward button — watching GPIOs 1,2,3,9,10,12,13,14,21,38,46");

    poll_pins!(
        delay,
        ("GPIO1", p1),
        ("GPIO2", p2),
        ("GPIO3", p3),
        ("GPIO9", p9),
        ("GPIO10", p10),
        ("GPIO12", p12),
        ("GPIO13", p13),
        ("GPIO14", p14),
        ("GPIO21", p21),
        ("GPIO38", p38),
        ("GPIO46", p46),
    );
}
