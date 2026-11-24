//! Moving test pattern, used for tests, benchmarks and headless sharing.

use std::time::Instant;

use super::RawFrame;

/// Generates frame `n` of a colour-bar pattern with a moving box and some
/// gradient detail (so the encoder has realistic work to do).
pub fn pattern(width: u32, height: u32, n: u32) -> RawFrame {
    let (w, h) = (width as usize, height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    const BARS: [[u8; 3]; 7] = [
        [192, 192, 192],
        [192, 192, 0],
        [0, 192, 192],
        [0, 192, 0],
        [192, 0, 192],
        [192, 0, 0],
        [0, 0, 192],
    ];
    let box_size = (h / 5).max(8);
    let max_x = w.saturating_sub(box_size).max(1);
    let max_y = h.saturating_sub(box_size).max(1);
    let bx = (n as usize * 7) % max_x;
    let by = (n as usize * 3) % max_y;
    for y in 0..h {
        let row = &mut rgba[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            let px = &mut row[x * 4..x * 4 + 4];
            let c = if x >= bx && x < bx + box_size && y >= by && y < by + box_size {
                [255, 255, 255]
            } else if y > h * 3 / 4 {
                let g = ((x * 255) / w.max(1)) as u8;
                [g, g.wrapping_add((n % 256) as u8), 255 - g]
            } else {
                BARS[(x * BARS.len()) / w.max(1)]
            };
            px[..3].copy_from_slice(&c);
            px[3] = 255;
        }
    }
    RawFrame {
        width,
        height,
        rgba,
        captured_at: Instant::now(),
    }
}
