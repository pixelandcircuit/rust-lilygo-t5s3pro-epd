use esp_hal::{
    dma::DmaTxBuf,
    dma_buffers,
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    lcd_cam::{
        lcd::{i8080, i8080::Command},
        LcdCam,
    },
    peripherals,
    rmt::PulseCode,
    time::Rate,
    Blocking,
};

use crate::driver::rmt;

// ── I2C device addresses ──────────────────────────────────────────────────────
const PCA9555_ADDR: u8 = 0x20;
const TPS65185_ADDR: u8 = 0x68;

// ── PCA9555 register map ──────────────────────────────────────────────────────
const PCA_REG_INPUT1: u8 = 0x01; // port-1 input (read)
const PCA_REG_OUTPUT0: u8 = 0x02; // port-0 output
const PCA_REG_OUTPUT1: u8 = 0x03; // port-1 output
const PCA_REG_CONFIG0: u8 = 0x06; // port-0 direction  (0=output)
const PCA_REG_CONFIG1: u8 = 0x07; // port-1 direction

// ── PCA9555 port-1 bit masks ──────────────────────────────────────────────────
const PCA_OE: u8 = 0x01; // bit 0 – display output-enable
const PCA_MODE: u8 = 0x02; // bit 1 – display mode
                           // bit 2 (STV) is an input on this board and not driven here
const PCA_PWRUP: u8 = 0x08; // bit 3 – TPS65185 power-up
const PCA_VCOM_CTRL: u8 = 0x10; // bit 4 – TPS65185 VCOM control
const PCA_WAKEUP: u8 = 0x20; // bit 5 – TPS65185 wakeup
const PCA_PWRGOOD: u8 = 0x40; // bit 6 – power-good flag (input)

// ── TPS65185 register map ─────────────────────────────────────────────────────
const TPS_REG_ENABLE: u8 = 0x01; // enable all rails (write 0x3F)
const TPS_REG_VCOM1: u8 = 0x03; // VCOM voltage LSB
const TPS_REG_VCOM2: u8 = 0x04; // VCOM voltage MSB
const TPS_REG_PG: u8 = 0x0F; // power-good status

const VCOM_MV: u16 = 1600;

const DMA_BUFFER_SIZE: usize = 248;

// ── Pulse helper (identical to V2.3 driver) ───────────────────────────────────
macro_rules! pulse {
    ($high:expr, $low:expr) => {
        if $high > 0 {
            [
                PulseCode::new(Level::High, $high, Level::Low, $low),
                PulseCode::end_marker(),
            ]
        } else {
            [
                PulseCode::new(Level::High, $low, Level::Low, 0),
                PulseCode::end_marker(),
            ]
        }
    };
}

// ── Pin config ────────────────────────────────────────────────────────────────

pub struct PinConfig<'a> {
    // 8-bit parallel data bus (D0–D7)
    pub data0: peripherals::GPIO5<'a>,
    pub data1: peripherals::GPIO6<'a>,
    pub data2: peripherals::GPIO7<'a>,
    pub data3: peripherals::GPIO15<'a>,
    pub data4: peripherals::GPIO16<'a>,
    pub data5: peripherals::GPIO17<'a>,
    pub data6: peripherals::GPIO18<'a>,
    pub data7: peripherals::GPIO8<'a>,
    // LCD control
    pub ckh: peripherals::GPIO4<'a>, // CKH – horizontal pixel clock → I8080 WRX
    pub sth: peripherals::GPIO41<'a>, // STH – start horizontal       → I8080 DC
    pub leh: peripherals::GPIO42<'a>, // LEH – latch-enable horizontal (GPIO)
    pub stv: peripherals::GPIO45<'a>, // STV – start vertical          (GPIO)
    // CKV row-clock (RMT)
    pub ckv: peripherals::GPIO48<'a>,
    // I2C bus shared by PCA9555 + TPS65185
    pub i2c_sda: peripherals::GPIO39<'a>,
    pub i2c_scl: peripherals::GPIO40<'a>,
}

// ── Main driver struct ────────────────────────────────────────────────────────

pub(crate) struct ED047TC1<'a> {
    i8080: Option<i8080::I8080<'a, Blocking>>,
    i2c: I2c<'a, Blocking>,
    leh: Output<'a>,
    stv: Output<'a>,
    rmt: rmt::Rmt<'a>,
    dma_buf: Option<DmaTxBuf>,
    pca_out1: u8,
}

impl<'a> ED047TC1<'a> {
    pub(crate) fn new(
        pins: PinConfig<'a>,
        dma: peripherals::DMA_CH0<'a>,
        lcd_cam: peripherals::LCD_CAM<'a>,
        rmt_periph: peripherals::RMT<'a>,
        i2c_periph: peripherals::I2C0<'a>,
    ) -> crate::driver::Result<Self> {
        // ── I2C ──────────────────────────────────────────────────────────────
        let mut i2c = I2c::new(i2c_periph, I2cConfig::default())
            .expect("to create I2C")
            .with_sda(pins.i2c_sda)
            .with_scl(pins.i2c_scl);

        // PCA9555 init:
        //   port 0 – all outputs, driven high (board-level signals)
        //   port 1 – bits 2/6/7 as inputs, rest outputs, driven low
        pca_write(&mut i2c, PCA_REG_CONFIG0, 0x00); // port 0: all output
        pca_write(&mut i2c, PCA_REG_CONFIG1, 0xC4); // port 1: bits 2,6,7 = input
        pca_write(&mut i2c, PCA_REG_OUTPUT0, 0xFF); // port 0: all high
        pca_write(&mut i2c, PCA_REG_OUTPUT1, 0x00); // port 1: all low

        // ── Direct GPIO ───────────────────────────────────────────────────────
        let leh = Output::new(pins.leh, Level::Low, OutputConfig::default());
        let stv = Output::new(pins.stv, Level::High, OutputConfig::default());

        // ── LCD I8080 ─────────────────────────────────────────────────────────
        let lcd_cam = LcdCam::new(lcd_cam);
        let (_, _, tx_buffer, tx_descriptors) = dma_buffers!(0, DMA_BUFFER_SIZE);
        let dma_buf = Some(
            DmaTxBuf::new(tx_descriptors, tx_buffer).map_err(crate::driver::Error::DmaBuffer)?,
        );

        let i8080_config = i8080::Config::default()
            .with_frequency(Rate::from_mhz(10))
            .with_cd_idle_edge(false)
            .with_cd_cmd_edge(true)
            .with_cd_dummy_edge(false)
            .with_cd_data_edge(false);

        let i8080 = Some(
            i8080::I8080::new(lcd_cam.lcd, dma, i8080_config)
                .expect("to create i8080 device")
                .with_dc(pins.sth) // STH (GPIO41) → start-horizontal pulse via DC
                .with_wrx(pins.ckh) // CKH (GPIO4)  → pixel clock
                .with_data0(pins.data0) // D0  (GPIO5)
                .with_data1(pins.data1) // D1  (GPIO6)
                .with_data2(pins.data2) // D2  (GPIO7)
                .with_data3(pins.data3) // D3  (GPIO15)
                .with_data4(pins.data4) // D4  (GPIO16)
                .with_data5(pins.data5) // D5  (GPIO17)
                .with_data6(pins.data6) // D6  (GPIO18)
                .with_data7(pins.data7), // D7  (GPIO8)
        );

        Ok(ED047TC1 {
            i8080,
            i2c,
            leh,
            stv,
            rmt: rmt::Rmt::new(rmt_periph),
            dma_buf,
            pca_out1: 0x00,
        })
    }

    // ── Power management ──────────────────────────────────────────────────────

    pub(crate) fn power_on(&mut self) {
        // 1. Assert WAKEUP
        self.pca_out1 = PCA_WAKEUP;
        self.pca_flush();

        // 2. Add PWRUP
        self.pca_out1 |= PCA_PWRUP;
        self.pca_flush();

        // 3. Add VCOM_CTRL
        self.pca_out1 |= PCA_VCOM_CTRL;
        self.pca_flush();

        // 4. Wait ~1 ms then poll PWRGOOD
        busy_delay(240_000);
        let mut tries = 0u32;
        while self.pca_read_port1() & PCA_PWRGOOD == 0 {
            busy_delay(24_000);
            tries += 1;
            if tries > 500 {
                break;
            }
        }

        // 5. Enable all TPS65185 power rails
        tps_write(&mut self.i2c, TPS_REG_ENABLE, 0x3F);

        // 6. Set VCOM = 1600 mV (val = 1600/10 = 160 = 0xA0)
        let val = VCOM_MV / 10;
        tps_write(&mut self.i2c, TPS_REG_VCOM2, (val >> 8) as u8);
        tps_write(&mut self.i2c, TPS_REG_VCOM1, (val & 0xFF) as u8);

        // 7. Wait for TPS power-good (PG & 0xFA == 0xFA)
        tries = 0;
        loop {
            if tps_read(&mut self.i2c, TPS_REG_PG) & 0xFA == 0xFA {
                break;
            }
            busy_delay(240_000);
            tries += 1;
            if tries >= 500 {
                break;
            }
        }
    }

    pub(crate) fn power_off(&mut self) {
        // Clear VCOM_CTRL, PWRUP, OE, MODE – hold WAKEUP briefly
        self.pca_out1 = PCA_WAKEUP;
        self.pca_flush();

        busy_delay(240_000); // ~1 ms

        self.pca_out1 = 0x00;
        self.pca_flush();
    }

    // ── Frame control ─────────────────────────────────────────────────────────

    pub(crate) fn frame_start(&mut self) -> crate::driver::Result<()> {
        // Enable MODE
        self.pca_out1 |= PCA_MODE;
        self.pca_flush();

        let data = pulse!(10, 10);
        self.rmt.pulse(&data, true)?;

        // STV low → high to latch the frame start
        self.stv.set_low();
        busy_delay(240);
        let data = pulse!(100, 100);
        let rmt_tx = self.rmt.pulse(&data, false)?;
        self.stv.set_high();
        if let Some(rmt_tx) = rmt_tx {
            self.rmt.reclaim_channel(rmt_tx)?;
        }

        let data = pulse!(0, 100);
        self.rmt.pulse(&data, true)?;

        // Enable OE
        self.pca_out1 |= PCA_OE;
        self.pca_flush();

        let data = pulse!(10, 10);
        self.rmt.pulse(&data, true)?;

        Ok(())
    }

    pub(crate) fn latch_row(&mut self) {
        self.leh.set_high();
        self.leh.set_low();
    }

    pub(crate) fn skip(&mut self) -> crate::driver::Result<()> {
        let data = pulse!(45, 5);
        self.rmt.pulse(&data, false)?;
        Ok(())
    }

    pub(crate) fn output_row(&mut self, output_time: u16) -> crate::driver::Result<()> {
        self.latch_row();

        let data = pulse!(output_time, 50);
        let rmt_tx = self.rmt.pulse(&data, false)?;
        let i8080 = self.i8080.take().ok_or(crate::driver::Error::Unknown)?;
        let dma_buf = self.dma_buf.take().ok_or(crate::driver::Error::Unknown)?;
        let tx = i8080
            .send(Command::<u8>::One(0), 0, dma_buf)
            .map_err(|(err, i8080, buf)| {
                self.dma_buf = Some(buf);
                self.i8080 = Some(i8080);
                crate::driver::Error::Dma(err)
            })?;
        let (r, i8080, dma_buf) = tx.wait();
        if let Some(rmt_tx) = rmt_tx {
            self.rmt.reclaim_channel(rmt_tx)?;
        }
        r.map_err(crate::driver::Error::Dma)?;
        self.i8080 = Some(i8080);
        self.dma_buf = Some(dma_buf);

        Ok(())
    }

    pub(crate) fn frame_end(&mut self) -> crate::driver::Result<()> {
        // Disable OE and MODE
        self.pca_out1 &= !(PCA_OE | PCA_MODE);
        self.pca_flush();

        let data = pulse!(10, 10);
        self.rmt.pulse(&data, true)?;
        self.rmt.pulse(&data, true)?;

        Ok(())
    }

    pub(crate) fn set_buffer(&mut self, data: &[u8]) -> crate::driver::Result<()> {
        let mut dma_buf = self.dma_buf.take().ok_or(crate::driver::Error::Unknown)?;
        dma_buf.as_mut_slice().fill(0);
        dma_buf.as_mut_slice()[..data.len()].copy_from_slice(data);
        self.dma_buf = Some(dma_buf);
        Ok(())
    }

    // ── I2C bus access (for touch controller sharing) ─────────────────────────

    pub(crate) fn i2c(&mut self) -> &mut I2c<'a, Blocking> {
        &mut self.i2c
    }

    // ── PCA9555 helpers ───────────────────────────────────────────────────────

    fn pca_flush(&mut self) {
        pca_write(&mut self.i2c, PCA_REG_OUTPUT1, self.pca_out1);
    }

    fn pca_read_port1(&mut self) -> u8 {
        pca_read(&mut self.i2c, PCA_REG_INPUT1)
    }
}

// ── Free-standing I2C helpers ─────────────────────────────────────────────────

fn pca_write(i2c: &mut I2c<'_, Blocking>, reg: u8, val: u8) {
    let _ = i2c.write(PCA9555_ADDR, &[reg, val]);
}

fn pca_read(i2c: &mut I2c<'_, Blocking>, reg: u8) -> u8 {
    let mut buf = [0u8; 1];
    let _ = i2c.write_read(PCA9555_ADDR, &[reg], &mut buf);
    buf[0]
}

fn tps_write(i2c: &mut I2c<'_, Blocking>, reg: u8, val: u8) {
    let _ = i2c.write(TPS65185_ADDR, &[reg, val]);
}

fn tps_read(i2c: &mut I2c<'_, Blocking>, reg: u8) -> u8 {
    let mut buf = [0u8; 1];
    let _ = i2c.write_read(TPS65185_ADDR, &[reg], &mut buf);
    buf[0]
}

// ── Timing ────────────────────────────────────────────────────────────────────

#[inline(always)]
fn busy_delay(wait_cycles: u32) {
    let target = cycles() + wait_cycles as u64;
    while cycles() < target {}
}

#[inline(always)]
fn cycles() -> u64 {
    esp_hal::xtensa_lx::timer::get_cycle_count() as u64
}
