//! Text → PNG rendering using the embedded glyph atlas.
//!
//! Strategy mirrors the Python proxy:
//!   - Hard-wrap input at 80 chars/line (kills column-blow-up from long lines).
//!   - Collapse runs of blank lines (each blank costs a full row).
//!   - Newspaper layout: pack lines into N columns so each image stays ≤ 1568px.
//!   - 2-color indexed PNG (paper=white, ink=black) — Anthropic bills by
//!     dimensions, not bytes, but tiny PNGs are nice for wire transfer.

use crate::font::AtlasFont;

const MAX_EDGE: u32 = 1568;
const COL_GAP_PX: u32 = 4;
const WRAP_CHARS: usize = 80;

pub struct Png {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub fn render_chunks(font: &AtlasFont, text: &str) -> Vec<Png> {
    // Minify: rstrip + collapse blank-line runs.
    let mut lines_raw: Vec<String> = Vec::new();
    let mut last_blank = false;
    for ln in text.lines() {
        let ln = ln.trim_end();
        if ln.is_empty() {
            if last_blank { continue; }
            last_blank = true;
            lines_raw.push(" ".to_string());
        } else {
            last_blank = false;
            lines_raw.push(ln.to_string());
        }
    }
    if lines_raw.is_empty() {
        lines_raw.push(" ".to_string());
    }

    // Hard-wrap to WRAP_CHARS.
    let mut lines: Vec<String> = Vec::new();
    for ln in lines_raw {
        if ln.len() <= WRAP_CHARS {
            lines.push(ln);
        } else {
            let bytes = ln.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let end = (i + WRAP_CHARS).min(bytes.len());
                lines.push(String::from_utf8_lossy(&bytes[i..end]).into_owned());
                i = end;
            }
        }
    }

    let cell_w = font.cell_w;
    let cell_h = font.cell_h;
    let col_w_px = WRAP_CHARS as u32 * cell_w + 1;
    let lines_per_col = ((MAX_EDGE / cell_h) as usize).max(8);

    let max_cols_per_img = ((MAX_EDGE / (col_w_px + COL_GAP_PX)).max(1)) as usize;
    let lines_per_img = lines_per_col * max_cols_per_img;

    let mut pngs: Vec<Png> = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let chunk_end = (start + lines_per_img).min(lines.len());
        let chunk = &lines[start..chunk_end];
        let c_needed = ((chunk.len() + lines_per_col - 1) / lines_per_col).max(1) as u32;
        let chunk_w = c_needed * col_w_px + (c_needed.saturating_sub(1)) * COL_GAP_PX;
        let last_col_lines = chunk.len() - ((c_needed - 1) as usize * lines_per_col);
        let tallest = if c_needed == 1 { last_col_lines } else { lines_per_col };
        let chunk_h = tallest as u32 * cell_h;

        let mut buf = vec![255u8; (chunk_w * chunk_h) as usize];
        for c in 0..c_needed as usize {
            let col_start = c * lines_per_col;
            let col_end = (col_start + lines_per_col).min(chunk.len());
            let x_base = c as u32 * (col_w_px + COL_GAP_PX);
            for (i, line) in chunk[col_start..col_end].iter().enumerate() {
                let y = i as u32 * cell_h;
                let mut x = x_base;
                for &ch in line.as_bytes() {
                    if x + cell_w > chunk_w { break; }
                    font.blit(ch, &mut buf, chunk_w, chunk_h, x, y);
                    x += cell_w;
                }
            }
        }

        let png_bytes = encode_indexed_png(&buf, chunk_w, chunk_h);
        pngs.push(Png { bytes: png_bytes, width: chunk_w, height: chunk_h });
        start = chunk_end;
    }
    pngs
}

/// Encode a u8 grayscale buffer as an 8-bit grayscale PNG.
/// Preserves antialiasing — critical: PIL+Menlo OCR'd at 99.7% because the
/// vision encoder uses subpixel grayscale gradients at tiny font sizes.
/// Deterministic: same input → same output bytes (so the cache hits).
fn encode_indexed_png(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(pixels.len() / 4 + 64);
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Best);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(pixels).expect("png data");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::AtlasFont;

    #[test]
    fn renders_short_text() {
        let font = AtlasFont::load(5.0).unwrap();
        let pngs = render_chunks(&font, "hello world\nsecond line");
        assert!(!pngs.is_empty());
        assert!(pngs[0].bytes.len() > 0);
        assert!(pngs[0].bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn renders_deterministically() {
        let font = AtlasFont::load(5.0).unwrap();
        let a = render_chunks(&font, "fibonacci(n-1) + fibonacci(n-2)");
        let b = render_chunks(&font, "fibonacci(n-1) + fibonacci(n-2)");
        assert_eq!(a.len(), b.len());
        for (a_png, b_png) in a.iter().zip(b.iter()) {
            assert_eq!(a_png.bytes, b_png.bytes, "PNG bytes must be deterministic for cache");
        }
    }
}
