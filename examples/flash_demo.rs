//! flash_demo — demonstrates persistent flash storage and boot-type detection.
//!
//! On every run it:
//!   1. Reads the hardware reset reason so you can distinguish power-on from
//!      software-reset from deep-sleep wakeup.
//!   2. Loads a boot counter from NVS flash via sequential-storage.
//!      `None` means the key has never been written (first-ever flash use).
//!      `Some(n)` means the board has booted at least once before.
//!   3. Increments and saves the counter.
//!   4. Saves and immediately reloads a second value to show the full
//!      store → fetch round-trip.
//!
//! Expected serial output (three resets in a row):
//!
//!   ─── flash_demo ──────────────────────────────────────────────────
//!   reset reason : power-on
//!   boot_count   : None → first flash use, writing 1
//!   last_value   : wrote 1000, read back Some(1000)
//!   ─────────────────────────────────────────────────────────────────
//!
//!   ─── flash_demo ──────────────────────────────────────────────────
//!   reset reason : software reset
//!   boot_count   : Some(1) → writing 2
//!   last_value   : wrote 2000, read back Some(2000)
//!   ─────────────────────────────────────────────────────────────────
//!
//! Keys 42 and 43 are used here so this example does not collide with
//! ereader_full (which uses key 0 in the same NVS partition).

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    main,
    rtc_cntl::{reset_reason, SocResetReason},
    system::Cpu,
};
use esp_println::println;
use esp_storage::FlashStorage;
use sequential_storage::{cache::NoCache, map};

esp_bootloader_esp_idf::esp_app_desc!();

// ── NVS partition range — must match partitions.csv ───────────────────────────
const NVS_FLASH_RANGE: core::ops::Range<u32> = 0x9000..0xF000;

// ── Keys used by this demo — different from ereader_full (key 0) ──────────────
const KEY_BOOT_COUNT: u8 = 42;
const KEY_LAST_VALUE: u8 = 43;

// ── FlashAdapter: bridges esp-storage's blocking NorFlash to the async ────────
// ── NorFlash trait required by sequential-storage 3.x ─────────────────────────
struct FlashAdapter(FlashStorage);

impl embedded_storage::nor_flash::ErrorType for FlashAdapter {
    type Error = esp_storage::FlashStorageError;
}

impl embedded_storage_async::nor_flash::ReadNorFlash for FlashAdapter {
    const READ_SIZE: usize = FlashStorage::WORD_SIZE as usize;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        embedded_storage::nor_flash::ReadNorFlash::read(&mut self.0, offset, bytes)
    }

    fn capacity(&self) -> usize {
        embedded_storage::nor_flash::ReadNorFlash::capacity(&self.0)
    }
}

impl embedded_storage_async::nor_flash::NorFlash for FlashAdapter {
    const WRITE_SIZE: usize = FlashStorage::WORD_SIZE as usize;
    const ERASE_SIZE: usize = FlashStorage::SECTOR_SIZE as usize;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        embedded_storage::nor_flash::NorFlash::erase(&mut self.0, from, to)
    }

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        embedded_storage::nor_flash::NorFlash::write(&mut self.0, offset, bytes)
    }
}

// ── Minimal no_std executor — flash ops never yield so this always exits ───────
// ── after one poll. ───────────────────────────────────────────────────────────
fn block_on<F: core::future::Future>(mut f: F) -> F::Output {
    use core::{
        pin::Pin,
        task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    };
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match unsafe { Pin::new_unchecked(&mut f) }.poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        }
    }
}

// ── Flash helpers for u32 values ──────────────────────────────────────────────

fn flash_load(key: u8) -> Option<u32> {
    let mut flash = FlashAdapter(FlashStorage::new());
    let mut cache = NoCache::new();
    let mut buf = [0u8; 64];
    match block_on(map::fetch_item::<u8, u32, _>(
        &mut flash,
        NVS_FLASH_RANGE,
        &mut cache,
        &mut buf,
        &key,
    )) {
        Ok(v) => v,
        Err(e) => {
            println!("flash_load: error {:?}", e);
            None
        }
    }
}

fn flash_save(key: u8, value: u32) {
    let mut flash = FlashAdapter(FlashStorage::new());
    let mut cache = NoCache::new();
    let mut buf = [0u8; 64];
    if let Err(e) = block_on(map::store_item::<u8, u32, _>(
        &mut flash,
        NVS_FLASH_RANGE,
        &mut cache,
        &mut buf,
        &key,
        &value,
    )) {
        println!("flash_save: error {:?}", e);
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────
#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();

    println!("─── flash_demo ──────────────────────────────────────────────────");

    // ── 1. Hardware reset reason ──────────────────────────────────────────────
    //
    // This tells you WHY the CPU reset, not just that it did. Useful for
    // distinguishing genuine power-on from a software crash or WDT bite.
    let reason = reset_reason(Cpu::ProCpu);
    let reason_str = match reason {
        Some(SocResetReason::ChipPowerOn) => "power-on",
        Some(SocResetReason::CoreDeepSleep) => "deep-sleep wakeup",
        Some(SocResetReason::CoreSw) => "software reset",
        Some(SocResetReason::CpuSw) => "CPU software reset",
        Some(SocResetReason::SysBrownOut) => "brownout",
        Some(SocResetReason::CoreMwdt0)
        | Some(SocResetReason::CoreMwdt1)
        | Some(SocResetReason::CpuMwdt0)
        | Some(SocResetReason::CpuMwdt1) => "watchdog",
        Some(SocResetReason::CoreRtcWdt)
        | Some(SocResetReason::CpuRtcWdt)
        | Some(SocResetReason::SysRtcWdt) => "RTC watchdog",
        _ => "unknown",
    };
    println!("reset reason : {}", reason_str);

    // ── 2. Load boot counter — None means the key has never been written ──────
    //
    // This is how you detect first-ever flash use vs. subsequent boots.
    // Hardware reset reason alone can't tell you this: a power-on after a
    // reflash looks the same as a power-on on a fresh device.
    let boot_count = flash_load(KEY_BOOT_COUNT);

    match boot_count {
        None => println!("boot_count   : None → first flash use, writing 1"),
        Some(n) => println!("boot_count   : Some({}) → writing {}", n, n + 1),
    }

    // ── 3. Increment and save ─────────────────────────────────────────────────
    let new_count = boot_count.unwrap_or(0) + 1;
    flash_save(KEY_BOOT_COUNT, new_count);

    // ── 4. Store and reload a second value (full round-trip demo) ────────────
    let payload: u32 = new_count * 1000;
    flash_save(KEY_LAST_VALUE, payload);
    let readback = flash_load(KEY_LAST_VALUE);
    println!("last_value   : wrote {}, read back {:?}", payload, readback);

    println!("─────────────────────────────────────────────────────────────────");
    println!("reset the board to see boot_count increment");

    loop {
        delay.delay_millis(1000);
    }
}
