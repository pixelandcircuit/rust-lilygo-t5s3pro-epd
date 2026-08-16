//! LoRa receive example — passively listens for packets on the onboard SX1262
//! and prints each received packet (hex + ASCII) to serial.
//!
//! SPI2 wiring (from official board definition):
//!   SCK=GPIO14  MOSI=GPIO13  MISO=GPIO21  CS=GPIO46
//!   RST=GPIO1   BUSY=GPIO47  DIO1=GPIO10
//!
//! The LoRa module shares the PCA9555 power rail with the GPS module
//! (I2C 0x20, port-0 bit 0). The same init as the gps example powers it on.
//!
//! Adjust the constants below to match the transmitter you want to monitor.
//! At minimum, FREQ_HZ must be correct — packets will not decode if the
//! frequency, spreading factor, or bandwidth differ from the transmitter.
//!
//! Run: cargo run --example lora_rx

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    spi::master::{Config as SpiConfig, Spi},
};
use esp_println::{print, println};

esp_bootloader_esp_idf::esp_app_desc!();

// ─── LoRa parameters — must match the transmitter ───────────────────────────
const FREQ_HZ: u64 = 915_000_000; // 915 MHz (US/AU); 868_000_000 for EU
const SF: u8 = 7; // Spreading factor: 5–12
const BW: u8 = 0x04; // Bandwidth: 0x04=125kHz  0x05=250kHz  0x06=500kHz
const CR: u8 = 0x01; // Coding rate: 4/5=0x01  4/6=0x02  4/7=0x03  4/8=0x04
                     // ────────────────────────────────────────────────────────────────────────────

// SX1262 command opcodes (Semtech SX1262 datasheet §13.1)
const SET_STANDBY: u8 = 0x80;
const SET_PACKET_TYPE: u8 = 0x01;
const SET_RF_FREQUENCY: u8 = 0x86;
const SET_MOD_PARAMS: u8 = 0x8B;
const SET_PKT_PARAMS: u8 = 0x8C;
const SET_DIO_IRQ_PARAMS: u8 = 0x08;
const SET_RX: u8 = 0x82;
const GET_IRQ_STATUS: u8 = 0x12;
const CLR_IRQ_STATUS: u8 = 0x02;
const GET_RX_BUF_STATUS: u8 = 0x13;
const GET_PKT_STATUS: u8 = 0x14;
const READ_BUFFER: u8 = 0x1E;

// IRQ flag bits (§13.3.2 Table 13-29)
const IRQ_RX_DONE: u16 = 1 << 1;
const IRQ_CRC_ERR: u16 = 1 << 6;
const IRQ_TIMEOUT: u16 = 1 << 9;

/// Compute the SX1262 RF frequency register value.
/// freq_reg = freq_hz × 2^25 / 32_000_000
fn freq_reg(hz: u64) -> [u8; 4] {
    let v = (hz << 25) / 32_000_000;
    [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8]
}

fn bw_label(bw: u8) -> &'static str {
    match bw {
        0x04 => "125",
        0x05 => "250",
        0x06 => "500",
        _ => "?",
    }
}

struct Radio<'d> {
    spi: Spi<'d, esp_hal::Blocking>,
    cs: Output<'d>,
    busy: Input<'d>,
}

impl<'d> Radio<'d> {
    fn wait_busy(&self) {
        while self.busy.is_high() {}
    }

    /// Send a write-only SPI command (no response expected).
    fn cmd(&mut self, data: &[u8]) {
        self.wait_busy();
        self.cs.set_low();
        self.spi.write(data).unwrap();
        self.cs.set_high();
    }

    /// Send a command and capture the response in-place.
    /// buf[0] = opcode; buf[1] = status byte after call; buf[2..] = response data.
    fn query(&mut self, buf: &mut [u8]) {
        self.wait_busy();
        self.cs.set_low();
        self.spi.transfer(buf).unwrap();
        self.cs.set_high();
    }
}

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    // Enable LoRa power via PCA9555 (port-0 bit 0 = LoRa+GPS rail).
    let mut i2c = I2c::new(peripherals.I2C0, I2cConfig::default())
        .expect("I2C init")
        .with_sda(peripherals.GPIO39)
        .with_scl(peripherals.GPIO40);
    let _ = i2c.write(0x20u8, &[0x06u8, 0x00u8]); // port-0: all outputs
    let _ = i2c.write(0x20u8, &[0x02u8, 0xFFu8]); // port-0: all high

    let delay = Delay::new();
    delay.delay_millis(100); // power-rail stabilisation

    let mut reset = Output::new(peripherals.GPIO1, Level::High, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO46, Level::High, OutputConfig::default());
    let busy = Input::new(peripherals.GPIO47, InputConfig::default());
    let dio1 = Input::new(peripherals.GPIO10, InputConfig::default());

    // SPI2 — default Config is 1 MHz which is well within SX1262's 16 MHz limit
    let spi = Spi::new(peripherals.SPI2, SpiConfig::default())
        .expect("SPI2 init")
        .with_sck(peripherals.GPIO14)
        .with_mosi(peripherals.GPIO13)
        .with_miso(peripherals.GPIO21);

    // Hardware reset: pull RST low for 2 ms, release, wait for BUSY to clear
    reset.set_low();
    delay.delay_millis(2);
    reset.set_high();
    delay.delay_millis(10);

    let mut radio = Radio { spi, cs, busy };
    radio.wait_busy();

    // Standby (STBY_RC) — chip is here after reset, but be explicit
    radio.cmd(&[SET_STANDBY, 0x00]);

    // Packet type = LoRa (0x01)
    radio.cmd(&[SET_PACKET_TYPE, 0x01]);

    // RF frequency
    let freq = freq_reg(FREQ_HZ);
    radio.cmd(&[SET_RF_FREQUENCY, freq[0], freq[1], freq[2], freq[3]]);

    // Modulation parameters: SF, BW, CR, LDRO
    // Low data-rate optimisation must be set when symbol duration > 16 ms
    // (SF11 or SF12 with BW 125 kHz).
    let ldro: u8 = if SF >= 11 && BW == 0x04 { 1 } else { 0 };
    radio.cmd(&[SET_MOD_PARAMS, SF, BW, CR, ldro]);

    // Packet parameters: preamble=12, explicit header, max payload=255, CRC on, standard IQ
    radio.cmd(&[SET_PKT_PARAMS, 0x00, 0x0C, 0x00, 0xFF, 0x01, 0x00]);

    // Route RxDone | CrcErr | Timeout to DIO1
    let irq_mask = IRQ_RX_DONE | IRQ_CRC_ERR | IRQ_TIMEOUT;
    let hi = (irq_mask >> 8) as u8;
    let lo = irq_mask as u8;
    radio.cmd(&[
        SET_DIO_IRQ_PARAMS,
        hi,
        lo, // global mask
        hi,
        lo, // DIO1
        0,
        0, // DIO2
        0,
        0, // DIO3
    ]);

    // Enter continuous receive (timeout=0xFFFFFF means never time out)
    radio.cmd(&[SET_RX, 0xFF, 0xFF, 0xFF]);

    println!(
        "[lora] {} Hz  SF{}  BW{} kHz  CR 4/{}  — waiting for packets...",
        FREQ_HZ,
        SF,
        bw_label(BW),
        CR + 4
    );

    let mut pkt_buf = [0u8; 259]; // 3-byte cmd header + up to 256 bytes payload
    let mut count: u32 = 0;

    loop {
        // Wait for DIO1 to go high (any enabled IRQ fired)
        while dio1.is_low() {}

        // GetIrqStatus → buf[2..4] = IRQ register
        let mut q = [GET_IRQ_STATUS, 0x00, 0x00, 0x00];
        radio.query(&mut q);
        let irq = ((q[2] as u16) << 8) | q[3] as u16;

        // Clear all IRQ flags
        radio.cmd(&[CLR_IRQ_STATUS, 0xFF, 0xFF]);

        if irq & IRQ_TIMEOUT != 0 {
            // Continuous mode (0xFFFFFF) should never reach here, but recover anyway
            radio.cmd(&[SET_RX, 0xFF, 0xFF, 0xFF]);
            continue;
        }

        if irq & IRQ_RX_DONE == 0 {
            continue;
        }

        // GetRxBufferStatus → buf[2]=payload_len, buf[3]=buffer_offset
        let mut q = [GET_RX_BUF_STATUS, 0x00, 0x00, 0x00];
        radio.query(&mut q);
        let payload_len = q[2] as usize;
        let rx_offset = q[3];

        // GetPacketStatus (LoRa) → buf[2]=RSSI_pkt, buf[3]=SNR_pkt, buf[4]=signal_RSSI
        let mut q = [GET_PKT_STATUS, 0x00, 0x00, 0x00, 0x00];
        radio.query(&mut q);
        let rssi = -(q[2] as i16) / 2; // dBm = -RssiPkt/2
        let snr = (q[3] as i8) as i16 / 4; // dB  =  SnrPkt/4 (signed)

        if irq & IRQ_CRC_ERR != 0 {
            println!("[lora] CRC error  rssi={} dBm  snr={} dB", rssi, snr);
            continue;
        }

        // ReadBuffer: [opcode, offset, NOP (status), payload...]
        let n = payload_len.min(pkt_buf.len() - 3);
        pkt_buf[0] = READ_BUFFER;
        pkt_buf[1] = rx_offset;
        pkt_buf[2] = 0x00; // NOP — SX1262 uses this byte for device status
        for b in pkt_buf[3..3 + n].iter_mut() {
            *b = 0x00;
        }
        radio.query(&mut pkt_buf[..3 + n]);
        let payload = &pkt_buf[3..3 + n];

        count += 1;
        println!(
            "[lora] #{:4}  len={:3}  rssi={:4} dBm  snr={:3} dB",
            count, n, rssi, snr
        );

        // Hex dump (16 bytes per line)
        for (i, &b) in payload.iter().enumerate() {
            if i % 16 == 0 {
                print!("        {:04x}: ", i);
            }
            print!("{:02x} ", b);
            if (i + 1) % 16 == 0 {
                println!();
            }
        }
        if n % 16 != 0 {
            println!();
        }

        // Printable ASCII? Show as text
        if payload.iter().all(|&b| b >= 0x20 && b < 0x7F) {
            if let Ok(s) = core::str::from_utf8(payload) {
                println!("        text: \"{}\"", s);
            }
        }
    }
}
