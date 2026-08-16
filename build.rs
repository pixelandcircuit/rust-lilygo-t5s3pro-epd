use std::{fs, path::PathBuf};

fn main() {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let is_xtensa = target_arch == "xtensa";

    if is_xtensa {
        linker_be_nice();
        println!("cargo:rustc-link-arg=-Tlinkall.x");
    }

    // Generate synthetic image data for the graphics_test example.
    // Both files use 4-bit grayscale packed BigEndian: high nibble = left pixel.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // 960×270 four-quadrant test card (129,600 bytes)
    fs::write(out.join("card.bin"), make_card(960, 270)).unwrap();

    // 960×50 horizontal gradient strip (24,000 bytes)
    fs::write(out.join("strip.bin"), make_strip(960, 50)).unwrap();

    println!("cargo:rerun-if-changed=build.rs");
}

/// Four-quadrant test card packed as 4-bit BigEndian Gray4.
fn make_card(w: usize, h: usize) -> Vec<u8> {
    let mut data = vec![0u8; w * h / 2];
    for y in 0..h {
        for x in 0..w {
            let pi = y * w + x;
            let luma = card_luma(x, y, w, h);
            if x % 2 == 0 {
                data[pi / 2] |= luma << 4; // high nibble = left pixel
            } else {
                data[pi / 2] |= luma; // low nibble  = right pixel
            }
        }
    }
    data
}

fn card_luma(x: usize, y: usize, w: usize, h: usize) -> u8 {
    let hw = w / 2;
    let hh = h / 2;
    if x < hw && y < hh {
        // Top-left: smooth horizontal gradient 0 → 15
        (x * 15 / (hw - 1)) as u8
    } else if x >= hw && y < hh {
        // Top-right: 8×8 checkerboard (black / white)
        if ((x / 8) + (y / 8)) % 2 == 0 {
            0
        } else {
            15
        }
    } else if x < hw {
        // Bottom-left: 16 solid vertical bands
        ((x * 16 / hw) % 16) as u8
    } else {
        // Bottom-right: concentric Chebyshev rectangles
        let cx = (x as isize - (hw + hw / 2) as isize).unsigned_abs();
        let cy = (y as isize - (hh + hh / 2) as isize).unsigned_abs();
        ((cx.max(cy) / 8) % 16) as u8
    }
}

/// 960-wide smooth gradient strip (same gradient repeated every row).
fn make_strip(w: usize, h: usize) -> Vec<u8> {
    let mut data = vec![0u8; w * h / 2];
    for y in 0..h {
        for x in 0..w {
            let pi = y * w + x;
            let luma = (x * 15 / (w - 1)) as u8;
            if x % 2 == 0 {
                data[pi / 2] |= luma << 4;
            } else {
                data[pi / 2] |= luma;
            }
        }
    }
    data
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];
        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                "_defmt_timestamp" => {
                    eprintln!();
                    eprintln!("💡 `defmt` not found — add `use defmt_rtt as _;` and `defmt.x` linker script");
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                _ => (),
            },
            _ => {
                std::process::exit(1);
            }
        }
        std::process::exit(0);
    }
    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
