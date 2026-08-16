//! SD card example — detects whether a micro-SD card is inserted, mounts
//! the FAT filesystem, and recursively lists all files and directories.
//!
//! The SD card shares SPI2 with the LoRa module but has its own CS pin:
//!   SCK=GPIO14  MOSI=GPIO13  MISO=GPIO21  CS=GPIO12
//!
//! Run: cargo run --example sd_list

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_sdmmc::{Error, SdCard, SdCardError, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    spi::master::{Config as SpiConfig, Spi},
    time::Instant,
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

struct DummyTimesource;
impl TimeSource for DummyTimesource {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// Recursively list directory contents, indented by depth.
/// MAX_DIRS must be > max expected nesting depth (each open subdir counts).
fn list_dir<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    dir: &embedded_sdmmc::Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    depth: u8,
) where
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
{
    const MAX_DEPTH: u8 = 8;
    if depth > MAX_DEPTH {
        println!("[sd]   (max depth reached)");
        return;
    }

    // 2 spaces per level, up to 32 chars
    const SPACES: &str = "                                ";
    let indent = &SPACES[..(depth as usize * 2).min(SPACES.len())];

    // Collect entries first — must not call VolumeManager methods inside
    // the iterate_dir callback (it holds an interior lock on the manager).
    let mut entries: Vec<(embedded_sdmmc::ShortFileName, bool, u32)> = Vec::new();
    let _ = dir.iterate_dir(|e| {
        // Skip LFN fragments and volume-label entries
        if e.attributes.is_lfn() || e.attributes.is_volume() {
            return;
        }
        // Skip . and .. pseudo-directories
        if e.name.base_name().first().copied() == Some(b'.') {
            return;
        }
        entries.push((e.name.clone(), e.attributes.is_directory(), e.size));
    });

    for (name, is_dir, size) in entries {
        if is_dir {
            println!("[sd] {}{}/", indent, name);
            // Open the subdir and recurse; subdir auto-closes on drop
            if let Ok(subdir) = dir.open_dir(name) {
                list_dir(&subdir, depth + 1);
            }
        } else {
            println!("[sd] {}{}  ({} bytes)", indent, name, size);
        }
    }
}

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    // Heap allocator — needed for Vec used in directory listing
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    // SPI2: shared bus with LoRa, but each peripheral has its own CS.
    // SD card CS is GPIO12; LoRa CS (GPIO46) is left floating high.
    let cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
    let spi = Spi::new(peripherals.SPI2, SpiConfig::default())
        .expect("SPI2 init")
        .with_sck(peripherals.GPIO14)
        .with_mosi(peripherals.GPIO13)
        .with_miso(peripherals.GPIO21);

    // ExclusiveDevice wraps the SpiBus + CS into a SpiDevice, which is what
    // embedded-sdmmc requires.
    let spi_dev = ExclusiveDevice::new(spi, cs, Delay::new()).unwrap();
    let sdcard = SdCard::new(spi_dev, Delay::new());

    // VolumeManager with MAX_DIRS=16 so deeply nested trees don't exhaust the
    // open-directory slots (each level of recursion holds one slot).
    let mgr = VolumeManager::<_, _, 16, 4, 1>::new_with_limits(sdcard, DummyTimesource, 0);

    // Try to open Volume 0 (first partition). This also triggers SD init.
    println!("[sd] looking for SD card...");
    let t0 = Instant::now();
    let vol = match mgr.open_volume(VolumeIdx(0)) {
        Ok(v) => {
            println!("[sd] card detected ({} ms)", t0.elapsed().as_millis());
            v
        }
        Err(Error::DeviceError(SdCardError::CardNotFound)) => {
            println!("[sd] no SD card detected ({} ms)", t0.elapsed().as_millis());
            loop {}
        }
        Err(e) => {
            println!("[sd] error ({} ms): {:?}", t0.elapsed().as_millis(), e);
            loop {}
        }
    };

    let root = match vol.open_root_dir() {
        Ok(d) => d,
        Err(e) => {
            println!("[sd] could not open root dir: {:?}", e);
            loop {}
        }
    };

    println!("[sd] /");
    let t1 = Instant::now();
    list_dir(&root, 1);
    println!("[sd] done ({} ms)", t1.elapsed().as_millis());

    loop {}
}
