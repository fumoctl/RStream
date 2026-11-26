//! Did the picture reach the screen? The player plays a clip of six known colour bars ([`y4m`]),
//! something outside the app captures what the OS actually shows (a screenshot), and [`find_bars`]
//! looks for the six bars, in order, side by side. A window that draws its controls but no video, a
//! black or see-through video layer, or a picture under the wrong window all fail, which a "the
//! screen isn't blank" check misses. (The idea comes from MediaMP's render-path matrix.)
//!
//! The clip is raw YUV (Y4M), which every FFmpeg plays without a decoder library. It tests the
//! picture's path to the screen, not hardware decoding.

/// The bars, left to right: three primaries then three secondaries. Every pair of neighbours differs
/// in at least two channels, so a blurred edge between them never reads as a bar of its own.
pub const BARS: [Bar; 6] = [Bar::Red, Bar::Green, Bar::Blue, Bar::Yellow, Bar::Magenta, Bar::Cyan];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar { Red, Green, Blue, Yellow, Magenta, Cyan }

impl Bar {
    /// Full-intensity RGB.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Bar::Red => [255, 0, 0],
            Bar::Green => [0, 255, 0],
            Bar::Blue => [0, 0, 255],
            Bar::Yellow => [255, 255, 0],
            Bar::Magenta => [255, 0, 255],
            Bar::Cyan => [0, 255, 255],
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Bar::Red => "red",
            Bar::Green => "green",
            Bar::Blue => "blue",
            Bar::Yellow => "yellow",
            Bar::Magenta => "magenta",
            Bar::Cyan => "cyan",
        }
    }
}

/// The clip's size: small (the player scales it to the window) and a multiple of 12, so every bar
/// is the same whole number of pixels and 4:2:0 chroma never straddles two bars.
pub const WIDTH: usize = 384;
pub const HEIGHT: usize = 216;

/// The colour bars as an uncompressed Y4M clip of `seconds` frames at one frame per second, in
/// limited-range BT.601 4:2:0 (what players assume for untagged standard-definition video).
pub fn y4m(seconds: usize) -> Vec<u8> {
    let (w, h) = (WIDTH, HEIGHT);
    let (cw, ch) = (w / 2, h / 2);
    let mut frame = Vec::with_capacity(6 + w * h + 2 * cw * ch);
    frame.extend_from_slice(b"FRAME\n");
    let bar_at = |x: usize| BARS[x * BARS.len() / w];
    for _ in 0..h {
        for x in 0..w { frame.push(ycbcr(bar_at(x).rgb()).0); }
    }
    for plane in [1, 2] {
        for _ in 0..ch {
            for x in 0..cw {
                let (_, cb, cr) = ycbcr(bar_at(x * 2).rgb());
                frame.push(if plane == 1 { cb } else { cr });
            }
        }
    }
    let header = format!("YUV4MPEG2 W{w} H{h} F1:1 Ip A1:1 C420jpeg XYSCSS=420JPEG\n");
    let mut out = Vec::with_capacity(header.len() + frame.len() * seconds);
    out.extend_from_slice(header.as_bytes());
    for _ in 0..seconds { out.extend_from_slice(&frame); }
    out
}

/// Full-range RGB to limited-range BT.601 Y'CbCr.
fn ycbcr([r, g, b]: [u8; 3]) -> (u8, u8, u8) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let y = 16.0 + 65.481 * r + 128.553 * g + 24.966 * b;
    let cb = 128.0 - 37.797 * r - 74.203 * g + 112.0 * b;
    let cr = 128.0 + 112.0 * r - 93.786 * g - 18.214 * b;
    let q = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    (q(y), q(cb), q(cr))
}

/// Which bar a screen pixel shows, if any: each channel must be clearly on or clearly off. Generous
/// on purpose, because colour management, a BT.601/709 mix-up or a dim screen shift the values.
pub fn classify([r, g, b]: [u8; 3]) -> Option<Bar> {
    let level = |c: u8| match c { 0..=100 => Some(false), 150..=255 => Some(true), _ => None };
    match (level(r)?, level(g)?, level(b)?) {
        (true, false, false) => Some(Bar::Red),
        (false, true, false) => Some(Bar::Green),
        (false, false, true) => Some(Bar::Blue),
        (true, true, false) => Some(Bar::Yellow),
        (true, false, true) => Some(Bar::Magenta),
        (false, true, true) => Some(Bar::Cyan),
        _ => None,
    }
}

/// Where the bars were found on the screenshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// Rows that show all six bars in order.
    pub rows: usize,
    pub left: usize,
    pub right: usize,
    pub top: usize,
    pub bottom: usize,
}

impl Found {
    pub fn summary(&self) -> String {
        format!("the six colour bars are on screen: {}x{} px at ({}, {}), on {} rows",
            self.right - self.left, self.bottom - self.top + 1, self.left, self.top, self.rows)
    }
}

/// The fewest matching rows that count as a picture, not a coincidence.
const MIN_ROWS: usize = 8;
/// The narrowest a bar may be on screen, in pixels.
const MIN_BAR: usize = 6;

/// Look for the six bars, in order and about equally wide, on an RGB (3 bytes per pixel) or RGBA
/// (4 bytes) screenshot. The player's controls may cover the top and bottom of the picture: a band of
/// rows in the middle is enough.
pub fn find_bars(pixels: &[u8], width: usize, height: usize, channels: usize) -> Result<Found, String> {
    if !(3..=4).contains(&channels) || pixels.len() < width * height * channels {
        return Err(format!("not a {width}x{height} image with {channels} channels ({} bytes)", pixels.len()));
    }
    let mut hits: Vec<(usize, usize, usize)> = Vec::new(); // (row, left, right)
    let mut best = 0;
    for y in 0..height {
        let row = &pixels[y * width * channels..(y + 1) * width * channels];
        let runs = runs(row, width, channels);
        match bars_in(&runs) {
            Ok((left, right)) => hits.push((y, left, right)),
            Err(matched) => best = best.max(matched),
        }
    }
    // The rows must agree on where the picture is (a few stray rows elsewhere don't count).
    let Some(&(_, l0, r0)) = median_by(&hits, |h| h.1) else {
        return Err(format!("no row shows the six colour bars (the best row had {best} of {} in order); {}",
            BARS.len(), dominant(pixels, width, height, channels)));
    };
    let tolerance = ((r0 - l0) / 50).max(4);
    let rows: Vec<_> = hits.iter().filter(|h| h.1.abs_diff(l0) <= tolerance && h.2.abs_diff(r0) <= tolerance).collect();
    if rows.len() < MIN_ROWS {
        return Err(format!("only {} row(s) show the six colour bars (need {MIN_ROWS})", rows.len()));
    }
    Ok(Found {
        rows: rows.len(),
        left: l0,
        right: r0,
        top: rows.iter().map(|h| h.0).min().unwrap_or(0),
        bottom: rows.iter().map(|h| h.0).max().unwrap_or(0),
    })
}

/// One row as runs of the same bar colour, with unclassified pixels and slivers dropped and the
/// neighbours they separated merged.
fn runs(row: &[u8], width: usize, channels: usize) -> Vec<(Bar, usize, usize)> {
    let mut raw: Vec<(Option<Bar>, usize, usize)> = Vec::new(); // (bar, start, end exclusive)
    for x in 0..width {
        let p = &row[x * channels..x * channels + 3];
        let bar = classify([p[0], p[1], p[2]]);
        match raw.last_mut() {
            Some(last) if last.0 == bar => last.2 = x + 1,
            _ => raw.push((bar, x, x + 1)),
        }
    }
    let mut out: Vec<(Bar, usize, usize)> = Vec::new();
    for (bar, start, end) in raw {
        let Some(bar) = bar else { continue };
        if end - start < MIN_BAR { continue; }
        match out.last_mut() {
            // The same colour on both sides of a short gap (an edge, a cursor): one run.
            Some(last) if last.0 == bar && start - last.2 <= MIN_BAR => last.2 = end,
            _ => out.push((bar, start, end)),
        }
    }
    out
}

/// The six bars as consecutive runs, touching (an edge's blur aside) and about equally wide:
/// their left and right edge, or how many of them in order the row got to at most.
fn bars_in(runs: &[(Bar, usize, usize)]) -> Result<(usize, usize), usize> {
    let mut best = 0;
    for start in 0..runs.len() {
        let mut matched = 0;
        while matched < BARS.len() && start + matched < runs.len() && runs[start + matched].0 == BARS[matched] {
            matched += 1;
        }
        best = best.max(matched);
        if matched < BARS.len() { continue; }
        let six = &runs[start..start + BARS.len()];
        let mut widths: Vec<usize> = six.iter().map(|r| r.2 - r.1).collect();
        widths.sort_unstable();
        let median = widths[widths.len() / 2];
        let even = six.iter().all(|r| { let w = r.2 - r.1; w * 10 >= median * 6 && w * 10 <= median * 16 });
        let touching = six.windows(2).all(|p| p[1].1 - p[0].2 <= (median / 4).max(MIN_BAR));
        if even && touching { return Ok((six[0].1, six[5].2)); }
    }
    Err(best)
}

fn median_by<T, K: Ord + Copy>(items: &[T], key: impl Fn(&T) -> K) -> Option<&T> {
    let mut sorted: Vec<&T> = items.iter().collect();
    sorted.sort_by_key(|t| key(t));
    sorted.get(sorted.len() / 2).copied()
}

/// What the screenshot mostly shows, for a failure message.
fn dominant(pixels: &[u8], width: usize, height: usize, channels: usize) -> String {
    let (mut dark, mut bars, mut total) = (0usize, 0usize, 0usize);
    for y in (0..height).step_by(4) {
        for x in (0..width).step_by(4) {
            let p = &pixels[(y * width + x) * channels..][..3];
            total += 1;
            if p.iter().all(|c| *c < 24) { dark += 1; }
            if classify([p[0], p[1], p[2]]).is_some() { bars += 1; }
        }
    }
    let pct = |n: usize| n * 100 / total.max(1);
    format!("{}% of the screen is near black, {}% has a bar colour", pct(dark), pct(bars))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screenshot: a dark desktop, the clip scaled into a window with a soft edge between bars,
    /// and a player control bar over its bottom part.
    fn screenshot(w: usize, h: usize, order: &[Bar]) -> Vec<u8> {
        let mut img = vec![18u8; w * h * 3];
        let (left, right, top, bottom) = (w / 8, w * 7 / 8, h / 6, h * 5 / 6);
        for y in top..bottom {
            for x in left..right {
                let bar_width = (right - left) as f64 / order.len() as f64;
                let pos = (x - left) as f64;
                let i = ((pos / bar_width) as usize).min(order.len() - 1);
                let mut c = order[i].rgb();
                // A three-pixel blend into the next bar, as a scaler leaves it.
                if i + 1 < order.len() && bar_width - (pos - i as f64 * bar_width) <= 3.0 {
                    let n = order[i + 1].rgb();
                    c = [0, 1, 2].map(|k| ((c[k] as u16 + n[k] as u16) / 2) as u8);
                }
                img[(y * w + x) * 3..][..3].copy_from_slice(&c);
            }
        }
        for y in bottom - (bottom - top) / 5..bottom {
            for x in left..right { img[(y * w + x) * 3..][..3].copy_from_slice(&[30, 30, 34]); }
        }
        img
    }

    #[test]
    fn finds_the_bars_under_the_controls() {
        let (w, h) = (1440, 900);
        let found = find_bars(&screenshot(w, h, &BARS), w, h, 3).unwrap();
        assert!(found.left.abs_diff(w / 8) <= 2 && found.right.abs_diff(w * 7 / 8) <= 2, "{found:?}");
        assert!(found.rows > 300, "{found:?}");
    }

    #[test]
    fn reads_rgba_too() {
        let (w, h) = (640, 360);
        let rgb = screenshot(w, h, &BARS);
        let rgba: Vec<u8> = rgb.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
        assert!(find_bars(&rgba, w, h, 4).is_ok());
    }

    #[test]
    fn a_black_video_area_fails() {
        let (w, h) = (800, 600);
        let err = find_bars(&vec![0u8; w * h * 3], w, h, 3).unwrap_err();
        assert!(err.contains("no row shows the six colour bars") && err.contains("100% of the screen is near black"), "{err}");
    }

    #[test]
    fn bars_in_another_order_fail() {
        let (w, h) = (800, 600);
        let shuffled = [Bar::Green, Bar::Red, Bar::Blue, Bar::Yellow, Bar::Magenta, Bar::Cyan];
        assert!(find_bars(&screenshot(w, h, &shuffled), w, h, 3).is_err());
    }

    #[test]
    fn half_the_bars_fail() {
        let (w, h) = (800, 600);
        assert!(find_bars(&screenshot(w, h, &BARS[..3]), w, h, 3).is_err());
    }

    #[test]
    fn a_few_stray_rows_are_not_a_picture() {
        let (w, h) = (800, 600);
        let mut img = vec![18u8; w * h * 3];
        let full = screenshot(w, h, &BARS);
        let y = h / 2;
        for row in y..y + 3 { img[row * w * 3..(row + 1) * w * 3].copy_from_slice(&full[row * w * 3..(row + 1) * w * 3]); }
        assert!(find_bars(&img, w, h, 3).unwrap_err().contains("only 3 row(s)"));
    }

    #[test]
    fn the_clip_has_the_bars_in_order() {
        let clip = y4m(2);
        let header = b"YUV4MPEG2 W384 H216 F1:1 Ip A1:1 C420jpeg XYSCSS=420JPEG\n";
        assert!(clip.starts_with(header));
        let frame = 6 + WIDTH * HEIGHT * 3 / 2;
        assert_eq!(clip.len(), header.len() + 2 * frame);
        // Decode the middle of each bar back to RGB (BT.601, limited range) and classify it.
        let data = &clip[header.len() + 6..];
        let (luma, chroma) = (WIDTH * HEIGHT, (WIDTH / 2) * (HEIGHT / 2));
        for (i, bar) in BARS.iter().enumerate() {
            let x = WIDTH * (2 * i + 1) / 12;
            let y = data[HEIGHT / 2 * WIDTH + x] as f64 - 16.0;
            let cb = data[luma + (HEIGHT / 4) * (WIDTH / 2) + x / 2] as f64 - 128.0;
            let cr = data[luma + chroma + (HEIGHT / 4) * (WIDTH / 2) + x / 2] as f64 - 128.0;
            let q = |v: f64| v.round().clamp(0.0, 255.0) as u8;
            let rgb = [q(1.164 * y + 1.596 * cr), q(1.164 * y - 0.392 * cb - 0.813 * cr), q(1.164 * y + 2.017 * cb)];
            assert_eq!(classify(rgb), Some(*bar), "bar {i} decodes to {rgb:?}");
        }
    }

    #[test]
    fn neighbour_blends_are_never_a_bar() {
        for pair in BARS.windows(2) {
            let (a, b) = (pair[0].rgb(), pair[1].rgb());
            let mid = [0, 1, 2].map(|k| ((a[k] as u16 + b[k] as u16) / 2) as u8);
            assert_eq!(classify(mid), None, "{:?}/{:?}", pair[0], pair[1]);
        }
    }
}
