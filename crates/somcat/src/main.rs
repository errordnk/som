//! `somcat` — a minimal terminal image viewer that speaks the Kitty
//! terminal graphics protocol directly, with no dependency on `crossterm`
//! or any other console-mode abstraction library.
//!
//! Written specifically after discovering (KITTY_GRAPHICS_PLAN.md Stage 7,
//! see `project_kitty_graphics_protocol` memory) that `yazi` (which relies
//! on `crossterm` for Windows console raw mode) fails to receive Som's
//! Kitty graphics query responses on Windows, because `crossterm`'s
//! `enable_raw_mode()` never sets `ENABLE_VIRTUAL_TERMINAL_INPUT` — without
//! that flag, the Windows console translates/discards raw VT byte
//! sequences instead of delivering them to a reading process's stdin. This
//! tool sets that flag itself, so it can serve as a genuinely reliable
//! reference client for testing Som's Kitty Graphics Protocol
//! implementation end-to-end, on Windows as well as Unix — independent of
//! whatever bugs/limitations third-party terminal libraries may have.
//!
//! Usage: `somcat <image-path> [<image-path> ...]`
//!
//! For each image: queries the terminal's Kitty graphics support (`a=q`),
//! waits briefly for a response, then transmits and displays the image
//! (`a=T`) at the current cursor position, sized to fit within a
//! reasonable terminal-cell footprint. Falls back to printing a plain
//! message (not garbage escape codes) if the terminal never answers the
//! capability query — the same graceful-degradation behavior real Kitty
//! graphics clients (`icat`, `yazi`) are expected to have, and that
//! `crates/terminal/src/kitty_graphics.rs`'s doc comments already describe
//! from Som's receiving side.
mod raw_mode;

use base64::Engine as _;
use std::io::{Read, Write};
use std::time::Duration;

/// Kitty's own recommended chunk size for base64 payloads split across
/// multiple APC strings — large enough to be efficient, small enough that
/// no real terminal's escape-sequence parser chokes on a single control
/// string. See <https://sw.kovidgoyal.net/kitty/graphics-protocol/#the-transmission-medium>.
const CHUNK_SIZE: usize = 4096;

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: somcat <image-path> [<image-path> ...]");
        std::process::exit(2);
    }

    let raw_guard = raw_mode::enable();
    let supported = query_kitty_support();
    if !supported {
        // Restore the terminal before printing anything further — no
        // point leaving it in raw mode just to print a fallback message.
        drop(raw_guard);
        eprintln!(
            "somcat: this terminal did not answer the Kitty graphics protocol capability query \
             (a=q) within the timeout — falling back to plain output instead of risking garbage \
             escape codes on screen."
        );
        for path in &paths {
            println!("[image: {path}] (Kitty graphics protocol not supported by this terminal)");
        }
        return;
    }

    let mut exit_code = 0;
    for path in &paths {
        if let Err(err) = display_image(path) {
            eprintln!("somcat: failed to display {path}: {err}");
            exit_code = 1;
        }
        // A blank line of vertical room after each image so multiple
        // images in one invocation don't visually overlap — the terminal
        // has already advanced its cursor past the image's cell footprint
        // by the point this runs (see display_image's placement sizing).
        println!();
    }

    drop(raw_guard);
    std::process::exit(exit_code);
}

/// Sends `a=q` and waits briefly for the terminal's response. Real Kitty
/// terminals answer within milliseconds; not answering at all within this
/// timeout is the terminal's own way of saying "I don't support this",
/// same assumption `KITTY_GRAPHICS_PLAN.md`'s degradation-check tests make
/// on the receiving side.
fn query_kitty_support() -> bool {
    print!("\x1b_Gi=1,a=q,t=d,f=24;AAAA\x1b\\");
    std::io::stdout().flush().ok();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut stdin = std::io::stdin();
        let marker = b"\x1b_Gi=1;OK\x1b\\";
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if buf.windows(marker.len()).any(|w| w == marker) || buf.len() >= 256 {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(Duration::from_millis(800)) {
        Ok(buf) => String::from_utf8_lossy(&buf).contains("\x1b_Gi=1;OK"),
        Err(_) => false,
    }
}

fn display_image(path: &str) -> Result<(), String> {
    let img = image::open(path).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let png_bytes = encode_png(&rgba, width, height)?;
    let base64_payload = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

    // A reasonable default cell footprint: cap at 40 columns / 20 rows so
    // a single image never floods an entire terminal window regardless of
    // its native pixel dimensions — Kitty's own c=/r= keys let the sender
    // request exactly this kind of display-size override independent of
    // the source image's real resolution.
    let columns = 40u32;
    let rows = 20u32;

    transmit_and_place(1, &base64_payload, columns, rows)
}

fn encode_png(rgba: &image::RgbaImage, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(rgba.clone())
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed for {width}x{height} image: {e}"))?;
    Ok(bytes)
}

/// Transmits the base64 PNG payload (chunked if needed, per Kitty's `m=`
/// continuation flag) and immediately places it at the current cursor
/// position with the given cell footprint.
fn transmit_and_place(id: u32, base64_payload: &str, columns: u32, rows: u32) -> Result<(), String> {
    let bytes = base64_payload.as_bytes();
    let mut offset = 0;
    let mut first = true;

    while offset < bytes.len() {
        let end = (offset + CHUNK_SIZE).min(bytes.len());
        let chunk = &bytes[offset..end];
        let more = end < bytes.len();
        let more_flag = if more { "1" } else { "0" };

        if first {
            print!(
                "\x1b_Ga=T,f=100,i={id},m={more_flag};{}\x1b\\",
                std::str::from_utf8(chunk).unwrap()
            );
            first = false;
        } else {
            print!("\x1b_Gm={more_flag};{}\x1b\\", std::str::from_utf8(chunk).unwrap());
        }
        offset = end;
    }

    print!("\x1b_Ga=p,i={id},p=1,c={columns},r={rows}\x1b\\");
    // Move the cursor past the placement's cell footprint so subsequent
    // output (the blank line in `main`, or a later image) doesn't
    // overlap it — Kitty placements don't reserve grid space on their
    // own, they're painted independent of the text layer.
    for _ in 0..rows {
        println!();
    }
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    Ok(())
}
