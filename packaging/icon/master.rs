//! The app icon master (`packaging/icon/flockswitch.png`, 1024×1024 RGBA) scaled to the
//! size the settings window wants. Used by build.rs.

use std::path::Path;

/// Decodes the master PNG and scales it to `size`×`size` RGBA by area averaging
/// (premultiplied, so transparent edges don't darken).
pub fn scaled(master: &Path, size: u32) -> Vec<u8> {
    let decoder = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(master).expect("opening icon master"),
    ));
    let mut reader = decoder.read_info().expect("reading icon master");
    let mut buf = vec![0; reader.output_buffer_size().expect("icon size")];
    let info = reader.next_frame(&mut buf).expect("decoding icon master");
    assert_eq!(info.color_type, png::ColorType::Rgba, "icon master must be RGBA");
    assert_eq!(info.bit_depth, png::BitDepth::Eight, "icon master must be 8-bit");
    let (w, h) = (info.width as usize, info.height as usize);
    let n = size as usize;

    let mut out = vec![0u8; n * n * 4];
    for oy in 0..n {
        let (y0, y1) = (oy * h / n, ((oy + 1) * h / n).max(oy * h / n + 1));
        for ox in 0..n {
            let (x0, x1) = (ox * w / n, ((ox + 1) * w / n).max(ox * w / n + 1));
            let mut acc = [0f64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &buf[(y * w + x) * 4..][..4];
                    let a = p[3] as f64 / 255.0;
                    acc[0] += p[0] as f64 * a;
                    acc[1] += p[1] as f64 * a;
                    acc[2] += p[2] as f64 * a;
                    acc[3] += a;
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as f64;
            let o = &mut out[(oy * n + ox) * 4..][..4];
            if acc[3] > 0.0 {
                for c in 0..3 {
                    o[c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u8;
                }
                o[3] = (acc[3] / count * 255.0).round() as u8;
            }
        }
    }
    out
}
