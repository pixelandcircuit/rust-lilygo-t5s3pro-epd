use embedded_hal::i2c::I2c;

pub const GT911_ADDR_PRIMARY: u8 = 0x5D;
pub const GT911_ADDR_ALT: u8 = 0x14;

const REG_COMMAND: [u8; 2] = [0x80, 0x40]; // command register
const REG_PRODUCT_ID: [u8; 2] = [0x81, 0x40]; // 4-byte ASCII product ID ("911\0")
const REG_STATUS: [u8; 2] = [0x81, 0x4E];
const REG_TOUCH0: [u8; 2] = [0x81, 0x50]; // first touch point: 8 bytes

// Config block: starts at 0x8047, 184 bytes of data, then checksum at 0x80FF, then 0x01 at 0x8100.
// Key offsets within the block (0-indexed from 0x8047):
//   0-1: X output max (little-endian)
//   2-3: Y output max (little-endian)
//   4:   Touch number (max fingers)
//   5:   Module_Switch1 — bits 2:0 = INT mode: 0=rising 1=falling 2=low-level 3=polling
//   6-183: various sensitivity / threshold settings (0 = chip default)
const CFG_REG: [u8; 2] = [0x80, 0x47]; // start of 184-byte config block
const CFG_FRESH_REG: [u8; 2] = [0x81, 0x00]; // write 0x01 here to reload config
const CFG_LEN: usize = 184; // bytes 0x8047–0x80FE

/// Minimal GT911 capacitive touch controller driver (polling, no INT pin).
pub struct Gt911 {
    pub addr: u8,
    x_max: u16,
    y_max: u16,
}

impl Gt911 {
    pub fn new(addr: u8) -> Self {
        Self {
            addr,
            x_max: 0,
            y_max: 0,
        }
    }

    /// Write a valid configuration so the GT911 starts scanning.
    /// The GT911 won't scan if its config block has an invalid checksum (version=0x00
    /// indicates the config has never been programmed).
    /// x_max / y_max should match the touch panel's coordinate range.
    pub fn configure<I: I2c>(&mut self, i2c: &mut I, x_max: u16, y_max: u16) {
        self.x_max = x_max;
        self.y_max = y_max;
        // Build the 184-byte config block (register 0x8047-0x80FE).
        // Offsets below are relative to 0x8047 (index 0 in this array).
        let mut cfg = [0u8; CFG_LEN];
        // Offsets 0-1: X output max
        cfg[0] = (x_max & 0xFF) as u8;
        cfg[1] = (x_max >> 8) as u8;
        // Offsets 2-3: Y output max
        cfg[2] = (y_max & 0xFF) as u8;
        cfg[3] = (y_max >> 8) as u8;
        // Offset 4: max touch points
        cfg[4] = 0x05;
        // Offset 5: Module_Switch1 — bits 2:0 = 1 (falling edge INT)
        // Use mode 1 instead of mode 3 (polling); in mode 3 some GT911 variants
        // only set buffer_ready when a touch occurs, making status look the same
        // as no-connection. Mode 1 uses the same status register with buffer_ready.
        cfg[5] = 0x01;
        // Offset 9 (0x8050): Large_touch_detect (0 = disabled)
        cfg[9] = 0x00;
        // Offset 10 (0x8051): Screen_Touch_Level — touch threshold.
        // 0x01 = minimum threshold (maximum sensitivity) for initial testing.
        cfg[10] = 0x01;
        // Offset 11 (0x8052): Screen_Leave_Level — release threshold
        cfg[11] = 0x10;
        // Offset 12 (0x8053): Low_Power_Control (0 = always active, no deep sleep)
        cfg[12] = 0x00;
        // Offset 13 (0x8054): Refresh_Rate — active scan period in ms (5 = 200 Hz)
        cfg[13] = 0x05;

        // Compute checksum: two's complement of the sum of all config bytes.
        let sum: u8 = cfg.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        let checksum = (!sum).wrapping_add(1);

        // Write config block: reg addr (2 bytes) + 184 bytes of data.
        // esp-hal I2C max transaction is typically 254 bytes; 184+2=186 is fine.
        let mut buf = [0u8; 2 + CFG_LEN];
        buf[0] = CFG_REG[0];
        buf[1] = CFG_REG[1];
        buf[2..].copy_from_slice(&cfg);
        let _ = i2c.write(self.addr, &buf);

        // Write checksum to 0x80FF.
        let _ = i2c.write(self.addr, &[0x80, 0xFF, checksum]);

        // Write 0x01 to 0x8100 to trigger config reload.
        let _ = i2c.write(self.addr, &[CFG_FRESH_REG[0], CFG_FRESH_REG[1], 0x01]);
    }

    /// Clear any stale buffer-ready flag and ensure the chip is in coordinate-output mode.
    pub fn init<I: I2c>(&mut self, i2c: &mut I) {
        // Ensure coordinate-output mode (0x00 = normal scanning)
        let _ = i2c.write(self.addr, &[REG_COMMAND[0], REG_COMMAND[1], 0x00]);
        // Clear any stale buffer-ready flag
        let _ = i2c.write(self.addr, &[REG_STATUS[0], REG_STATUS[1], 0x00]);
    }

    /// Read the 4-byte product ID and return it. "911\0" confirms a real GT911.
    pub fn product_id<I: I2c>(&mut self, i2c: &mut I) -> [u8; 4] {
        let mut id = [0u8; 4];
        let _ = i2c.write_read(self.addr, &REG_PRODUCT_ID, &mut id);
        id
    }

    /// Returns the (x, y) coordinates of the first active touch point, or None.
    pub fn read_touch<I: I2c>(&mut self, i2c: &mut I) -> Option<(u16, u16)> {
        let mut status = [0u8; 1];
        i2c.write_read(self.addr, &REG_STATUS, &mut status).ok()?;

        let count = status[0] & 0x0F;

        // Always clear the buffer-ready flag so the GT911 can write new touch
        // data on the next scan cycle — must happen even when count == 0.
        let _ = i2c.write(self.addr, &[REG_STATUS[0], REG_STATUS[1], 0x00]);

        if count == 0 {
            return None;
        }

        let mut pt = [0u8; 8];
        i2c.write_read(self.addr, &REG_TOUCH0, &mut pt).ok()?;

        // Byte layout (empirically verified):
        // [0]=Y_low [1]=Y_high [2]=X_low [3]=X_high [4]=touch_area_low [5]=touch_area_high
        // X is in 0..x_max (correct orientation).
        // Y is physically inverted: raw y=y_max is the physical top of the screen.
        let x = u16::from_le_bytes([pt[2], pt[3]]).min(self.x_max);
        let y_raw = u16::from_le_bytes([pt[0], pt[1]]).min(self.y_max);
        let y = self.y_max - y_raw;
        Some((x, y))
    }

    /// Read the raw status register byte without clearing it (for diagnostics only).
    pub fn read_status_raw<I: I2c>(&mut self, i2c: &mut I) -> u8 {
        let mut status = [0u8; 1];
        let _ = i2c.write_read(self.addr, &REG_STATUS, &mut status);
        status[0]
    }

    /// Write 0x00 to the status register to clear the buffer-ready flag.
    pub fn clear_status<I: I2c>(&mut self, i2c: &mut I) {
        let _ = i2c.write(self.addr, &[REG_STATUS[0], REG_STATUS[1], 0x00]);
    }

    /// Read key config registers and return them for diagnostics.
    /// Returns [version, x_lo, x_hi, y_lo, y_hi, max_touch, int_mode]
    pub fn read_config<I: I2c>(&mut self, i2c: &mut I) -> [u8; 7] {
        let mut cfg = [0u8; 7];
        let _ = i2c.write_read(self.addr, &[0x80, 0x46], &mut cfg);
        cfg
    }

    /// Probe both known GT911 addresses and return the one that responds.
    pub fn detect<I: I2c>(i2c: &mut I) -> Option<u8> {
        for &addr in &[GT911_ADDR_PRIMARY, GT911_ADDR_ALT] {
            let mut buf = [0u8; 1];
            if i2c.write_read(addr, &REG_STATUS, &mut buf).is_ok() {
                return Some(addr);
            }
        }
        None
    }
}
