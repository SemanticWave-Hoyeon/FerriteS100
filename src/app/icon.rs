//! Load the window icon from `./icon.ico` and decode the most reasonable size.
//!
//! `winit::Icon` wants raw RGBA, so we parse the ICO directory ourselves
//! (PNG-payload icons go through the `image` crate; BMP/DIB-payload icons are
//! decoded inline with vertical flip + BGRA→RGBA swap).

use std::fs;
use std::path::PathBuf;

use tracing::{info, warn};
use winit::window::Icon;

/// Load `./icon.ico` and convert it to a winit `Icon`. Returns `None` if the
/// file is missing or unparseable; the caller should treat that as non-fatal.
pub fn load_window_icon() -> Option<Icon> {
    let icon_path = PathBuf::from("./icon.ico");

    if !icon_path.exists() {
        warn!("Icon file not found: {}", icon_path.display());
        return None;
    }

    match fs::read(&icon_path) {
        Ok(data) => match parse_ico_to_rgba(&data) {
            Some((rgba, width, height)) => match Icon::from_rgba(rgba, width, height) {
                Ok(icon) => {
                    info!("Window icon loaded: {}x{}", width, height);
                    Some(icon)
                }
                Err(e) => {
                    warn!("Failed to create icon: {}", e);
                    None
                }
            },
            None => {
                warn!("Failed to parse ICO file");
                None
            }
        },
        Err(e) => {
            warn!("Failed to read icon file: {}", e);
            None
        }
    }
}

/// Parse an ICO byte buffer and return the chosen image as `(rgba, width, height)`.
///
/// ICO file structure:
/// - Header (6 bytes): reserved, type, image count
/// - Directory entries (16 bytes each): width, height, colors, reserved, planes, bpp, size, offset
/// - Image data (BMP/DIB or PNG)
fn parse_ico_to_rgba(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if data.len() < 6 {
        return None;
    }

    // Check ICO header
    let _reserved = u16::from_le_bytes([data[0], data[1]]);
    let image_type = u16::from_le_bytes([data[2], data[3]]);
    let image_count = u16::from_le_bytes([data[4], data[5]]);

    if image_type != 1 || image_count == 0 {
        return None;
    }

    // Find the best icon — prefer ones that fit within 48×48 (the size winit
    // typically displays); fall back to whatever is largest if none qualify.
    let mut best_entry: Option<(usize, u32, u32, u32, u32)> = None;

    for i in 0..image_count as usize {
        let entry_offset = 6 + i * 16;
        if entry_offset + 16 > data.len() {
            break;
        }

        // Width and height (0 means 256)
        let width = if data[entry_offset] == 0 {
            256u32
        } else {
            data[entry_offset] as u32
        };
        let height = if data[entry_offset + 1] == 0 {
            256u32
        } else {
            data[entry_offset + 1] as u32
        };
        let size = u32::from_le_bytes([
            data[entry_offset + 8],
            data[entry_offset + 9],
            data[entry_offset + 10],
            data[entry_offset + 11],
        ]);
        let offset = u32::from_le_bytes([
            data[entry_offset + 12],
            data[entry_offset + 13],
            data[entry_offset + 14],
            data[entry_offset + 15],
        ]);

        // Prefer larger icons, but not too large (32×32 or 48×48 ideal)
        let score = width * height;
        if best_entry.is_none() || score <= 48 * 48 {
            best_entry = Some((i, width, height, size, offset));
        }
    }

    let (_, _width, _height, size, offset) = best_entry?;
    let offset = offset as usize;
    let size = size as usize;

    if offset + size > data.len() {
        return None;
    }

    let image_data = &data[offset..offset + size];

    // PNG-payload icons start with the PNG signature.
    if image_data.len() >= 8 && &image_data[0..8] == b"\x89PNG\r\n\x1a\n" {
        use image::GenericImageView;
        match image::load_from_memory(image_data) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = img.dimensions();
                Some((rgba.into_raw(), w, h))
            }
            Err(_) => None,
        }
    } else {
        // DIB (BMP without file header). Height is doubled because the
        // payload includes an AND mask after the colour bitmap.
        if image_data.len() < 40 {
            return None;
        }

        let header_size =
            u32::from_le_bytes([image_data[0], image_data[1], image_data[2], image_data[3]]);

        if header_size < 40 {
            return None;
        }

        let dib_width =
            i32::from_le_bytes([image_data[4], image_data[5], image_data[6], image_data[7]]) as u32;

        let dib_height =
            i32::from_le_bytes([image_data[8], image_data[9], image_data[10], image_data[11]])
                .unsigned_abs()
                / 2;

        let bpp = u16::from_le_bytes([image_data[14], image_data[15]]);

        // Only handle 32-bit BGRA — the common case for modern .ico files.
        if bpp != 32 {
            return None;
        }

        let pixel_offset = header_size as usize;
        let row_size = (dib_width * 4) as usize;
        let pixel_data_size = row_size * dib_height as usize;

        if pixel_offset + pixel_data_size > image_data.len() {
            return None;
        }

        // Convert BGRA→RGBA and flip vertically (DIB is bottom-up).
        let mut rgba = vec![0u8; (dib_width * dib_height * 4) as usize];

        for y in 0..dib_height {
            let src_y = (dib_height - 1 - y) as usize;
            let src_offset = pixel_offset + src_y * row_size;
            let dst_offset = (y * dib_width * 4) as usize;

            for x in 0..dib_width {
                let src_px = src_offset + (x as usize) * 4;
                let dst_px = dst_offset + (x as usize) * 4;

                if src_px + 4 <= image_data.len() {
                    rgba[dst_px] = image_data[src_px + 2]; // R
                    rgba[dst_px + 1] = image_data[src_px + 1]; // G
                    rgba[dst_px + 2] = image_data[src_px]; // B
                    rgba[dst_px + 3] = image_data[src_px + 3]; // A
                }
            }
        }

        Some((rgba, dib_width, dib_height))
    }
}
