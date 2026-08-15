use alloc::{boxed::Box, vec, vec::Vec};

use esp_hal::{delay::Delay, peripherals};
use log::*;

use crate::driver::{ed047tc1, Error, Result};

const CONTRAST_CYCLES_4BPP: &[u16; 15] = &[
    30, 30, 20, 20, 30, 30, 30, 40, 40, 50, 50, 50, 100, 200, 300,
];
const CONTRAST_CYCLES_4BPP_WHITE: &[u16; 15] =
    &[10, 10, 8, 8, 8, 8, 8, 10, 10, 15, 15, 20, 20, 100, 300];

/// Display rotation, only 90° increments supported
#[derive(Clone, Copy, Default)]
pub enum DisplayRotation {
    #[default]
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
}

#[derive(Clone, Copy, Debug)]
pub enum DrawMode {
    BlackOnWhite,
    WhiteOnWhite,
    WhiteOnBlack,
}

#[derive(Clone, Copy, Debug)]
pub struct Rectangle {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl DrawMode {
    fn lut_default(&self) -> u8 {
        match self {
            Self::BlackOnWhite => 0x55,
            Self::WhiteOnBlack | Self::WhiteOnWhite => 0xAA,
        }
    }

    fn contrast_cycles(&self) -> &[u16; 15] {
        match self {
            Self::WhiteOnBlack => CONTRAST_CYCLES_4BPP_WHITE,
            Self::BlackOnWhite | Self::WhiteOnWhite => CONTRAST_CYCLES_4BPP,
        }
    }
}

const TAINTED_ROWS_SIZE: usize = Display::HEIGHT as usize / 8 + 1;
const FRAMEBUFFER_SIZE: usize = (Display::WIDTH / 2) as usize * Display::HEIGHT as usize;
const BYTES_PER_LINE: usize = Display::WIDTH as usize / 4;
const LINE_BYTES_4BPP: usize = Display::WIDTH as usize / 2;

pub struct Display<'a> {
    epd: ed047tc1::ED047TC1<'a>,
    skipping: u16,
    framebuffer: Box<[u8; FRAMEBUFFER_SIZE]>,
    tainted_rows: [u8; TAINTED_ROWS_SIZE],
    rotation: DisplayRotation,
}

impl<'a> Display<'a> {
    pub const WIDTH: u16 = 960;
    pub const HEIGHT: u16 = 540;
    pub const BOUNDING_BOX: Rectangle = Rectangle {
        x: 0,
        y: 0,
        width: Self::WIDTH,
        height: Self::HEIGHT,
    };

    pub fn new(
        pins: ed047tc1::PinConfig<'a>,
        dma: peripherals::DMA_CH0<'a>,
        lcd_cam: peripherals::LCD_CAM<'a>,
        rmt: peripherals::RMT<'a>,
        i2c: peripherals::I2C0<'a>,
    ) -> Result<Self> {
        Ok(Display {
            epd: ed047tc1::ED047TC1::new(pins, dma, lcd_cam, rmt, i2c)?,
            skipping: 0,
            framebuffer: Box::new([0xFF; FRAMEBUFFER_SIZE]),
            tainted_rows: [0; TAINTED_ROWS_SIZE],
            rotation: DisplayRotation::default(),
        })
    }

    pub fn set_rotation(&mut self, rotation: DisplayRotation) {
        self.rotation = rotation;
    }

    pub fn rotation(&self) -> DisplayRotation {
        self.rotation
    }

    pub fn power_on(&mut self) {
        debug!("Display power on");
        self.epd.power_on()
    }

    pub fn power_off(&mut self) {
        debug!("Display power off");
        self.epd.power_off()
    }

    pub fn set_pixel(&mut self, x: u16, y: u16, color: u8) -> Result<()> {
        if x >= Self::WIDTH || y >= Self::HEIGHT {
            return Err(Error::OutOfBounds);
        }
        if color > 0x0F {
            return Err(Error::InvalidColor);
        }
        let index: usize = x as usize / 2 + y as usize * (Self::WIDTH as usize / 2);
        let value = self.framebuffer[index];
        if x % 2 == 1 {
            self.framebuffer[index] = (value & 0x0F) | ((color << 4) & 0xF0);
        } else {
            self.framebuffer[index] = (value & 0xF0) | (color & 0x0F);
        }
        let tainted_index = y as usize / 8;
        self.tainted_rows[tainted_index] |= 1 << (y % 8);
        Ok(())
    }

    /// Raw framebuffer bytes for screenshot capture.
    ///
    /// Layout: 4 bpp Gray4, 2 pixels per byte, row-major.
    /// Byte at `x/2 + y * (WIDTH/2)`: low nibble = even column, high nibble = odd column.
    /// Values 0x0 (black) … 0xF (white).
    ///
    /// **Important**: `flush()` resets the framebuffer to `0xFF` (all-white) after
    /// sending to the panel. Capture after `render_*()` but before `flush()`.
    pub fn framebuffer(&self) -> &[u8] {
        &*self.framebuffer
    }

    pub fn fill(&mut self, color: u8) -> Result<()> {
        debug!("display fill");
        if color > 0x0F {
            return Err(Error::InvalidColor);
        }
        self.framebuffer.fill(color << 4 | color);
        self.tainted_rows.fill(0xFF);
        Ok(())
    }

    pub fn flush(&mut self, mode: DrawMode) -> Result<()> {
        debug!("display flush");
        self.draw_inner(mode, 0, Self::WIDTH)?;
        self.tainted_rows.fill(0);
        self.framebuffer.fill(0xFF);
        Ok(())
    }

    /// Like `flush`, but confines the waveform to `clip`.
    ///
    /// Pixels outside the clip column range receive VCOM (no drive) even if
    /// their row is dirty. This prevents ghost bands when only a sub-width
    /// rectangle is being updated: the driver always transmits full rows, but
    /// out-of-clip pixels are masked to 0b00 in the DMA buffer so the panel
    /// never actually drives them.
    ///
    /// Row-axis clipping is still handled by the dirty-row bitmap as usual,
    /// so only rows within `clip.y .. clip.y + clip.height` need to be tainted.
    pub fn flush_clip(&mut self, mode: DrawMode, clip: Rectangle) -> Result<()> {
        debug!("display flush_clip");
        let x_end = clip.x.saturating_add(clip.width).min(Self::WIDTH);
        self.draw_inner(mode, clip.x, x_end)?;
        self.tainted_rows.fill(0);
        self.framebuffer.fill(0xFF);
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        debug!("display clear");
        self.clear_area(Self::BOUNDING_BOX)
    }

    /// Poll the GT911 touch controller for the first active touch point.
    /// Returns `Some((x, y))` when a finger is down, `None` otherwise.
    pub fn read_touch(&mut self, gt911: &mut crate::driver::gt911::Gt911) -> Option<(u16, u16)> {
        gt911.read_touch(self.epd.i2c())
    }

    /// Write a valid GT911 configuration block so it starts scanning.
    /// Call this if the GT911 was never configured (config version = 0x00).
    pub fn configure_touch(&mut self, gt911: &mut crate::driver::gt911::Gt911, x_max: u16, y_max: u16) {
        gt911.configure(self.epd.i2c(), x_max, y_max);
    }

    /// Clear the GT911 buffer-ready flag and set coordinate-output mode.
    pub fn init_touch(&mut self, gt911: &mut crate::driver::gt911::Gt911) {
        gt911.init(self.epd.i2c());
    }

    /// Read the GT911 4-byte product ID ("911\0" if genuine).
    pub fn touch_product_id(&mut self, gt911: &mut crate::driver::gt911::Gt911) -> [u8; 4] {
        gt911.product_id(self.epd.i2c())
    }

    /// Probe both GT911 I2C addresses and return the one that ACKs.
    /// Returns `None` if no touch controller is found on the bus.
    pub fn detect_touch_addr(&mut self) -> Option<u8> {
        crate::driver::gt911::Gt911::detect(self.epd.i2c())
    }

    /// Read GT911 config registers for diagnostics.
    /// Returns [version, x_lo, x_hi, y_lo, y_hi, max_touch, int_mode]
    pub fn touch_read_config(&mut self, gt911: &mut crate::driver::gt911::Gt911) -> [u8; 7] {
        gt911.read_config(self.epd.i2c())
    }

    /// Read the raw GT911 status register byte for diagnostics.
    pub fn touch_read_status_raw(&mut self, gt911: &mut crate::driver::gt911::Gt911) -> u8 {
        gt911.read_status_raw(self.epd.i2c())
    }

    /// Write 0x00 to the GT911 status register to clear the buffer-ready flag.
    pub fn touch_clear_status(&mut self, gt911: &mut crate::driver::gt911::Gt911) {
        gt911.clear_status(self.epd.i2c())
    }

    /// Read one byte from an arbitrary I2C device on the shared bus.
    pub fn i2c_read_u8(&mut self, addr: u8, reg: u8) -> u8 {
        let i2c = self.epd.i2c();
        let mut buf = [0u8; 1];
        let _ = i2c.write_read(addr, &[reg], &mut buf);
        buf[0]
    }

    /// Read two bytes (little-endian u16) from an arbitrary I2C device on the shared bus.
    pub fn i2c_read_u16(&mut self, addr: u8, reg: u8) -> u16 {
        let i2c = self.epd.i2c();
        let mut buf = [0u8; 2];
        let _ = i2c.write_read(addr, &[reg], &mut buf);
        u16::from_le_bytes(buf)
    }

    /// Read two bytes (little-endian i16) from an arbitrary I2C device on the shared bus.
    pub fn i2c_read_i16(&mut self, addr: u8, reg: u8) -> i16 {
        let i2c = self.epd.i2c();
        let mut buf = [0u8; 2];
        let _ = i2c.write_read(addr, &[reg], &mut buf);
        i16::from_le_bytes(buf)
    }

    /// Scan all 7-bit I2C addresses and print those that ACK (for diagnostics).
    pub fn i2c_scan(&mut self) {
        let i2c = self.epd.i2c();
        for addr in 0x00u8..=0x7F {
            let mut buf = [0u8; 1];
            if i2c.read(addr, &mut buf).is_ok() {
                esp_println::println!("  I2C ACK at 0x{:02X}", addr);
            }
        }
    }

    pub fn repair(&mut self, delay: Delay) -> Result<()> {
        debug!("display repair");
        self.clear()?;
        for _ in 0..20 {
            self.push_pixels(Self::BOUNDING_BOX, 50, 0)?;
            delay.delay_millis(500);
        }
        self.clear()?;
        for _ in 0..40 {
            self.push_pixels(Self::BOUNDING_BOX, 50, 1)?;
            delay.delay_millis(500);
        }
        self.clear()
    }

    pub fn clear_area(&mut self, area: Rectangle) -> Result<()> {
        self.clear_cycles(area, 4, 50)
    }

    fn clear_cycles(&mut self, area: Rectangle, cycles: u16, cycle_time: u16) -> Result<()> {
        for _ in 0..cycles {
            for _ in 0..4 {
                self.push_pixels(area, cycle_time, 0)?;
            }
            for _ in 0..4 {
                self.push_pixels(area, cycle_time, 1)?;
            }
        }
        Ok(())
    }

    fn push_pixels(&mut self, area: Rectangle, time: u16, color: u16) -> Result<()> {
        let mut row = [0u8; BYTES_PER_LINE];

        for i in 0..area.width {
            let pos = i + area.x % 4;
            let mask = match color {
                1 => 0b10101010,
                _ => 0b01010101,
            } & (0b00000011 << (2 * (pos % 4)));
            row[(area.x / 4 + pos / 4) as usize] |= mask;
        }
        line_buffer_reorder(&mut row);
        self.epd.frame_start()?;

        for i in 0..Self::HEIGHT {
            if i < area.y {
                self.row_skip(time)?;
                continue;
            }
            if i == area.y {
                self.epd.set_buffer(&row)?;
                self.row_write(time)?;
                continue;
            }
            if i >= area.y + area.height {
                self.row_skip(time)?;
                continue;
            }
            self.row_write(time)?;
        }
        self.row_write(time)?;
        self.epd.frame_end()?;

        Ok(())
    }

    fn row_skip(&mut self, output_time: u16) -> Result<()> {
        match self.skipping {
            0 => {
                self.epd.set_buffer(&[0u8; BYTES_PER_LINE])?;
                self.epd.output_row(output_time)?;
            }
            i if i < 2 => {
                self.epd.output_row(10)?;
            }
            _ => {
                self.epd.skip()?;
            }
        }
        self.skipping += 1;
        Ok(())
    }

    fn row_write(&mut self, output_time: u16) -> Result<()> {
        self.skipping = 0;
        self.epd.output_row(output_time)?;
        Ok(())
    }

    fn is_tainted(&self, row: u16) -> bool {
        let index = row as usize / 8;
        self.tainted_rows[index] & (1 << (row % 8)) != 0
    }

    const DRAW_IMAGE_FRAME_COUNT: usize = 15;

    fn draw_inner(&mut self, mode: DrawMode, clip_x_start: u16, clip_x_end: u16) -> Result<()> {
        let clipping = clip_x_start > 0 || clip_x_end < Self::WIDTH;
        let mut lut = vec![mode.lut_default(); 1 << 16];

        for k in 0..Self::DRAW_IMAGE_FRAME_COUNT {
            update_lut(&mut lut, k, mode);
            self.skipping = 0;
            self.epd.frame_start()?;
            for y in 0..Self::HEIGHT {
                if !self.is_tainted(y) {
                    self.row_skip(mode.contrast_cycles()[k])?;
                    continue;
                }
                let start = y as usize * LINE_BYTES_4BPP;
                let end = start + LINE_BYTES_4BPP;
                let mut buf = prepare_dma_buffer(&self.framebuffer[start..end], &lut);
                if clipping {
                    clip_dma_buffer(&mut buf, clip_x_start, clip_x_end);
                }
                self.epd.set_buffer(buf.as_slice())?;
                self.row_write(mode.contrast_cycles()[k])?;
            }
            if self.skipping == 0 {
                self.row_write(mode.contrast_cycles()[k])?;
            }
            self.epd.frame_end()?;
        }
        Ok(())
    }
}

fn line_buffer_reorder(data: &mut [u8]) {
    for chunk in data.chunks_exact_mut(4) {
        let val = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let swapped = (val >> 16) | ((val & 0x0000FFFF) << 16);
        chunk.copy_from_slice(&swapped.to_le_bytes());
    }
}

fn prepare_dma_buffer(line_data: &[u8], conversion_lut: &[u8]) -> Vec<u8> {
    let mut epd_input = vec![0u8; BYTES_PER_LINE];
    let mut wide_epd_input: Vec<u32> = vec![0u32; Display::WIDTH as usize / 16];

    let line_data_16: Vec<u16> = line_data
        .chunks(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    for (j, chunk) in line_data_16.chunks(4).enumerate() {
        if let [v1, v2, v3, v4] = chunk {
            let pixel: u32 = (conversion_lut[*v1 as usize] as u32)
                | (conversion_lut[*v2 as usize] as u32) << 8
                | (conversion_lut[*v3 as usize] as u32) << 16
                | (conversion_lut[*v4 as usize] as u32) << 24;
            wide_epd_input[j] = pixel;
        }
    }

    for (i, &wide_pixel) in wide_epd_input.iter().enumerate() {
        epd_input[i * 4..(i + 1) * 4].copy_from_slice(&wide_pixel.to_le_bytes());
    }

    // ED047TC1 expects MSB-first within each byte (bits 6-7 = leftmost pixel).
    // The LUT produces LSB-first (bits 0-1 = leftmost), so reverse the 2-bit pair order.
    for byte in epd_input.iter_mut() {
        let b = *byte;
        *byte = ((b & 0x03) << 6)
              | ((b & 0x0C) << 2)
              | ((b & 0x30) >> 2)
              | ((b & 0xC0) >> 6);
    }

    epd_input
}

/// Zero out the waveform drive codes for pixels outside [x_start, x_end).
///
/// Each byte in the post-bit-reversal DMA buffer encodes 4 pixels:
///   bits 7-6 = pixel b*4+0, bits 5-4 = b*4+1, bits 3-2 = b*4+2, bits 1-0 = b*4+3
/// Setting a 2-bit pair to 0b00 outputs VCOM (no drive) for that pixel.
fn clip_dma_buffer(buf: &mut [u8], x_start: u16, x_end: u16) {
    for (b, byte) in buf.iter_mut().enumerate() {
        let px_base = b as u16 * 4;
        if px_base + 4 <= x_start || px_base >= x_end {
            *byte = 0x00;
        } else if px_base < x_start || px_base + 4 > x_end {
            for p in 0u16..4 {
                let px = px_base + p;
                if px < x_start || px >= x_end {
                    // p=0 → bits 7-6 (shift 6), p=1 → 5-4 (shift 4), etc.
                    *byte &= !(0b11u8 << (6 - p as u8 * 2));
                }
            }
        }
    }
}

fn update_lut(conversion_lut: &mut [u8], k: usize, mode: DrawMode) {
    let k = match mode {
        DrawMode::BlackOnWhite | DrawMode::WhiteOnWhite => Display::DRAW_IMAGE_FRAME_COUNT - k,
        DrawMode::WhiteOnBlack => k,
    };
    for l in (k..1 << 16).step_by(16) {
        conversion_lut[l] &= 0xFC;
    }
    for l in ((k << 4)..(1 << 16)).step_by(1 << 8) {
        for p in 0..16 {
            conversion_lut[l + p] &= 0xF3
        }
    }
    for l in ((k << 8)..(1 << 16)).step_by(1 << 12) {
        for p in 0..(1 << 8) {
            conversion_lut[l + p] &= 0xCF
        }
    }
    for l in (k << 12)..((k + 1) << 12) {
        conversion_lut[l] &= 0x3F;
    }
}
