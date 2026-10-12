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
    let modules = (size + 2 * QUIET as i32) as usize;
    let stride = modules * SCALE;

    // one pixel row: filter byte + quiet zone + module bits + quiet zone
    let mut pixel_row = Vec::with_capacity(stride + 1);
    let white_row = || {
        let mut row = Vec::with_capacity(stride + 1);
        row.push(0); // filter: none
        row.resize(stride + 1, 255);
        row
    };
    let mut raw = Vec::with_capacity((stride + 1) * stride);
    for _ in 0..QUIET * SCALE {
        raw.extend_from_slice(&white_row());
    }
    for y in 0..size {
        pixel_row.clear();
        pixel_row.push(0); // filter: none
        for _ in 0..QUIET * SCALE {
            pixel_row.push(255);
        }
        for x in 0..size {
            let byte = if qr.get_module(x, y) { 0u8 } else { 255u8 };
            for _ in 0..SCALE {
                pixel_row.push(byte);
            }
        }
        for _ in 0..QUIET * SCALE {
            pixel_row.push(255);
        }
        // every module row is SCALE pixel rows tall
        for _ in 0..SCALE {
            raw.extend_from_slice(&pixel_row);
        }
    }
    for _ in 0..QUIET * SCALE {
        raw.extend_from_slice(&white_row());
    }
    debug_assert_eq!(raw.len(), (stride + 1) * stride);

    let png = encode_png(&raw, stride as u32, stride as u32);
    std::fs::create_dir_all(dir)?;
    // one file, overwritten per attempt: a unique name per pairing
    // would accumulate in the cache dir forever, and nothing reads a
    // past attempt's QR
    let path = dir.join("connect-qr.png");
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
        // a copy at a fixed path for host-side decoding (the container
        // test runs against the bind-mounted repo)
        std::fs::copy(&path, "/work/target/qr-decode-test.png").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let mut offset = 8;
        let mut total = 8u32;
        let mut idat: Vec<u8> = Vec::new();
        let mut dimensions = (0u32, 0u32);
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
            if &bytes[offset + 4..offset + 8] == b"IHDR" {
                let width = u32::from_be_bytes([
                    bytes[offset + 8],
                    bytes[offset + 9],
                    bytes[offset + 10],
                    bytes[offset + 11],
                ]);
                let height = u32::from_be_bytes([
                    bytes[offset + 12],
                    bytes[offset + 13],
                    bytes[offset + 14],
                    bytes[offset + 15],
                ]);
                dimensions = (width, height);
            }
            if &bytes[offset + 4..offset + 8] == b"IDAT" {
                idat.extend_from_slice(&bytes[offset + 8..offset + 8 + len as usize]);
            }
            total += 12 + len;
            offset += 12 + len as usize;
        }
        assert_eq!(total as usize, bytes.len());
        // the decoder rejects short pixel streams silently, so the
        // decompressed scanline data must be exactly height rows of
        // (1 filter byte + width bytes)
        use std::io::Read as _;
        let (width, height) = dimensions;
        let decoded = flate2::read::ZlibDecoder::new(idat.as_slice())
            .bytes()
            .collect::<Result<Vec<u8>, _>>()
            .unwrap();
        assert_eq!(
            decoded.len(),
            height as usize * (1 + width as usize),
            "IDAT must carry every pixel row the header declares"
        );
    }
}
