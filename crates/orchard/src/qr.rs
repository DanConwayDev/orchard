//! QR rendering for NIP-46 sign-in: encode the pairing URI, rasterize
//! it to a small PNG on disk. gpui shows it with the same `img()` path
//! the screenshot pipeline uses.
//!
//! The PNG is written by hand: eight-bit grayscale, one row per scan
//! line, zlib via flate2, CRCs per the PNG spec. No image crate.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use qrcodegen::QrCode;

/// Pixels per module and the quiet-zone border, in modules. The quiet
/// zone is part of the QR spec: scanners need the white margin.
const SCALE: usize = 8;
const QUIET: usize = 4;

/// Encode `text` and write the QR image. Returns the file path (unique
/// per call, so the image cache never shows a stale pairing).
pub fn write_qr_png(text: &str, dir: &std::path::Path) -> Result<PathBuf> {
    let qr = QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium)?;
    let size = qr.size();
    let stride = ((size + 2 * QUIET as i32) * SCALE as i32) as usize;

    // grayscale rows: 255 white, 0 black, each module SCALE rows
    let mut raw = Vec::with_capacity((stride + 1) * stride);
    let quiet_row = vec![255u8; stride + 1];
    for _ in 0..QUIET {
        raw.extend_from_slice(&quiet_row);
    }
    for y in 0..size {
        let mut row = Vec::with_capacity(stride + 1);
        row.push(0); // filter: none
        for _ in 0..QUIET as i32 * SCALE as i32 {
            row.push(255);
        }
        for x in 0..size {
            let byte = if qr.get_module(x, y) { 0u8 } else { 255u8 };
            for _ in 0..SCALE {
                row.push(byte);
            }
        }
        for _ in 0..QUIET as i32 * SCALE as i32 {
            row.push(255);
        }
        raw.extend_from_slice(&row);
    }
    for _ in 0..QUIET {
        raw.extend_from_slice(&quiet_row);
    }

    let png = encode_png(&raw, stride as u32, stride as u32);
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        "connect-qr-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::write(&path, png)?;
    Ok(path)
}

/// A minimal PNG: IHDR (8-bit grayscale), one zlib IDAT, IEND.
fn encode_png(raw: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(0); // color type: grayscale
    ihdr.push(0); // compression: zlib
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    chunk(&mut out, b"IHDR", &ihdr);

    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    let _ = z.write_all(raw);
    let idat = z.finish().unwrap_or_default();
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(body);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PNG must be structurally valid: signature, chunk sizes
    /// summing to the file, a CRC that actually covers its chunk.
    #[test]
    fn qr_png_is_wellformed() {
        let dir = std::env::temp_dir().join("orchard-qr-test");
        let path = write_qr_png(
            "nostrconnect://0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?relay=ws%3A%2F%2F127.0.0.1%3A8008&name=Orchard",
            &dir,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let mut offset = 8;
        let mut total = 8u32;
        while offset < bytes.len() {
            let len = u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            let mut crc = flate2::Crc::new();
            crc.update(&bytes[offset + 4..offset + 8 + len as usize]);
            assert_eq!(
                crc.sum(),
                u32::from_be_bytes([
                    bytes[offset + 8 + len as usize],
                    bytes[offset + 9 + len as usize],
                    bytes[offset + 10 + len as usize],
                    bytes[offset + 11 + len as usize],
                ])
            );
            total += 12 + len;
            offset += 12 + len as usize;
        }
        assert_eq!(total as usize, bytes.len());
        let _ = std::fs::remove_file(&path);
    }
}
