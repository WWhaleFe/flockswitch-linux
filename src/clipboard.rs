//! Long-lived selection owner. GNOME uses arboard's XWayland fallback; compositors
//! offering data-control use the native Wayland clipboard.
use std::{path::PathBuf, sync::Mutex};

#[derive(Default)]
pub struct Clipboard(Mutex<Option<arboard::Clipboard>>);
impl Clipboard {
    fn with<T>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Option<T> {
        let mut owner = self.0.lock().ok()?;
        if owner.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => *owner = Some(c),
                Err(e) => { log::warn!("clipboard unavailable: {e}"); return None; }
            }
        }
        match f(owner.as_mut()?) {
            Ok(value) => Some(value),
            Err(arboard::Error::ContentNotAvailable) => None,
            Err(e) => { log::warn!("clipboard: {e}"); None }
        }
    }
    pub fn text(&self) -> Option<String> { self.with(|c| c.get_text()) }
    pub fn set_text(&self, text: &str) { self.with(|c| c.set_text(text)); }
    pub fn files(&self) -> Vec<PathBuf> {
        // Only a real file copy (text/uri-list) counts: copied text that merely looks
        // like a file URI must never send that file's contents along.
        self.with(|c| c.get().file_list()).unwrap_or_default()
    }
    pub fn set_files(&self, files: &[PathBuf]) { self.with(|c| c.set().file_list(files)); }
    pub fn image(&self) -> Option<Vec<u8>> {
        let image = self.with(|c| c.get_image())?;
        encode_png(image.width, image.height, &image.bytes)
    }
    pub fn set_image(&self, png: &[u8]) {
        let Some((width, height, bytes)) = decode_png(png) else {
            log::warn!("clipboard: invalid or oversized PNG"); return;
        };
        self.with(|c| c.set_image(arboard::ImageData { width, height, bytes: bytes.into() }));
    }
}

const MAX_BYTES: usize = 100 * 1024 * 1024;
fn rgba_len(width: usize, height: usize) -> Option<usize> {
    let len = width.checked_mul(height)?.checked_mul(4)?;
    (width > 0 && height > 0 && len <= MAX_BYTES).then_some(len)
}
pub fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    if rgba.len() != rgba_len(width, height)? { return None; }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().ok()?.write_image_data(rgba).ok()?;
    (out.len() <= MAX_BYTES).then_some(out)
}
pub fn decode_png(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    if data.len() > MAX_BYTES { return None; }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_limits(png::Limits { bytes: MAX_BYTES });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width as usize, reader.info().height as usize);
    rgba_len(w, h)?;
    let size = reader.output_buffer_size()?;
    if size > MAX_BYTES { return None; }
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buf,
        (png::ColorType::Rgb, png::BitDepth::Eight) => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        _ => return None,
    };
    Some((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn png_round_trip() {
        let pixels: Vec<u8> = (0..24).collect();
        assert_eq!(decode_png(&encode_png(2, 3, &pixels).unwrap()), Some((2, 3, pixels)));
        assert_eq!(decode_png(b"invalid"), None);
    }
    #[test] fn png_limits_and_shape() {
        assert!(encode_png(usize::MAX, 2, &[]).is_none());
        assert!(encode_png(0, 2, &[]).is_none());
        assert!(encode_png(1, 1, &[0; 3]).is_none());
        assert!(rgba_len(10000, 10000).is_none());
    }
}
