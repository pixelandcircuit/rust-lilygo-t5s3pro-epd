# Minesweeper refresh and VCOM notes

## Running and local dependencies

Run `cargo run --example minesweeper` to build, flash, and monitor the game.
The 9x9 board has ten mines, Reveal/Flag modes, a safe first reveal, empty-region
flood fill, win/loss detection, and New Game. Squares use ASCII cell symbols.
Touch input is re-armed by an explicit zero-contact GT911 report; absence of a
fresh report is not treated as a finger release.

The repository expects sibling checkouts at `../iris-ui` (crate `iris-ui`) and
`../embedded-inspect/embedded-inspect`. The former replaces the obsolete
`../rust-embedded-gui` dependency path.

## Current refresh policy

Before an action, the example snapshots cell symbols, game state, flag count,
and tap mode. It compares these with the new state to decide what to repaint.

- A single-cell change repaints its 48x48 square. Changes within the same board
  row are batched into a bounding rectangle, restoring all cells inside it.
- Partial repaint first erases the region with `WhiteOnBlack`, then draws its
  new content with `BlackOnWhite`. Both passes use `flush_clip` to mask columns;
  drawing only the region marks only its rows dirty.
- Mode buttons and status are repainted only when their displayed state changes.
- Changes spanning multiple board rows use a full hardware clear and redraw.
- The sixth update since the last full refresh also uses a full clear/redraw.
  `FULL_REFRESH_INTERVAL` controls this interval; it must be at least 1. Setting
  it to 1 makes every accepted update use a full refresh. Mode changes count
  toward the interval; ignored taps do not.
- Display power is turned off after initial rendering and after each update,
  and restored before the next update. Serial logs report refresh type and time.

Full refreshes visibly flash and cost more time. This is a conservative
maintenance policy, not a calibrated partial-refresh waveform.

## Driver changes and hardware observations

`ED047TC1::skip` now waits for the RMT transaction to finish. Previously the
asynchronous transaction was discarded, leaving the channel unavailable and
causing subsequent pulses to reinitialize it.

Skipped stretches also disable source output-enable after draining the pending
row. `output_row` latches the preceding DMA transfer, so that pending row must
finish before blanking. `row_write` re-enables outputs; unchanged output-enable
states avoid redundant I2C writes. Hardware clear frames reset the skip counter.

On-device testing reported less fading after these changes, but untouched
regions still faded or darkened over repeated partial refreshes. Repainting the
header alone did not address drift in the grid and has been replaced by the full
refresh policy above. The user reported the maintenance policy was much better.
Long-term drift-free operation and exact refresh timings have not been verified.

## VCOM: known values and remaining uncertainty

The Rust driver still uses `VCOM_MV = 1600`, programming a nominal **-1.60 V**.
No voltage change was made during this work.

The [LILYGO product page](https://lilygo.cc/en-us/products/t5-e-paper-s3-pro)
identifies the ED047TC1 panel but does not specify VCOM. In the official linked
repository, both the
[display example](https://github.com/Xinyuan-LilyGO/T5S3-4.7-e-paper-PRO/blob/938d6189db0fde9a12558629bc1405bb49e1be78/examples/display_test/main/main.cpp#L68)
and the
[LVGL example](https://github.com/Xinyuan-LilyGO/T5S3-4.7-e-paper-PRO/blob/938d6189db0fde9a12558629bc1405bb49e1be78/examples/lvgl_test/main/main.cpp#L222)
call `epd_set_vcom(1560)`. The API takes a positive magnitude for the negative
voltage, so those examples use **-1.56 V**, 40 mV different from our setting.
This is a manufacturer example value, not confirmation of the calibration of
this individual display. Its printed VCOM value was not accessible.

[EPDiy troubleshooting](https://github.com/vroland/epdiy#troubleshooting)
specifically associates fading/darkening outside partial updates with VCOM
calibration. Its [calibration documentation](https://epdiy.readthedocs.io/en/latest/getting_started.html#calibrate-vcom)
explains that the value is panel-dependent. VCOM is therefore a plausible cause,
but has not been established as the cause of this device's behavior. Comparing
the manufacturer example value against the current setting would be a separate
hardware experiment; retain the maintenance refresh policy until verified.

## Validation

`cargo build --example minesweeper` builds and links for ESP32-S3. Existing
`iris-ui` unused-code/import warnings and an RWX LOAD-segment linker warning
remain. Compilation does not validate electrical timing or panel calibration.

The hardware-independent rules have five passing tests:

```sh
rustc +stable --edition=2021 --test examples/minesweeper/game.rs -o /tmp/minesweeper-tests
/tmp/minesweeper-tests
```

They cover first-reveal safety and exact mine count across multiple seeds and
starting cells, flag protection/removal, flood fill, edge neighbors, win/loss,
and reset. The game and current refresh workaround were also exercised on the
user's device.
