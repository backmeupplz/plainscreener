//! Candlestick chart rendered to an SVG string.
//!
//! One implementation, three outputs: the server inlines it in pages (so the chart
//! paints before any JS runs), rasterizes it for OG images, and the browser runs the
//! same code as wasm for panning, zooming and the crosshair. Colors come from CSS
//! classes (`g` grid, `u`/`d` up/down, `a` axis text, `x` crosshair) so the page theme
//! applies without re-rendering.

// max/min instead of clamp: clamp's panic message drags float formatting into the wasm.
#![allow(clippy::manual_clamp)]

pub mod fmt;
#[cfg(target_arch = "wasm32")]
mod wasm;

/// One OHLCV candle. `repr(C)` so the browser can hand over a `Float64Array` as-is.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Candle {
    pub t: f64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct View {
    pub w: f64,
    pub h: f64,
    /// Candle length in seconds.
    pub res: f64,
    /// Fractional index of the leftmost visible slot.
    pub start: f64,
    /// Number of visible candle slots.
    pub count: f64,
    pub cursor: Option<(f64, f64)>,
    /// Seconds added to timestamps for labels (the viewer's UTC offset).
    pub tz: f64,
}

/// Width of the price axis on the right.
pub const AXIS_W: f64 = 72.0;
const AXIS_H: f64 = 22.0;
const LEGEND_H: f64 = 26.0;
const VOLUME: f64 = 0.2;
/// Approximate width of a 12 px label character.
const CHAR_W: f64 = 7.2;
/// Time-axis label steps in seconds.
const STEPS: [f64; 14] = [
    60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0, 14_400.0, 21_600.0, 43_200.0, 86_400.0, 172_800.0, 604_800.0,
    2_592_000.0, 31_536_000.0,
];

impl View {
    /// The default view: the latest candles at ~9 px each, with two empty slots on the right.
    pub fn latest(n: usize, w: f64, h: f64, res: f64) -> View {
        // ~9 px per candle, but a young pool's few candles shouldn't hug the right edge.
        let count = ((w - AXIS_W) / 9.0).min(n as f64 + 12.0).max(24.0).min(400.0);
        View { w, h, res, start: n as f64 + 2.0 - count, count, cursor: None, tz: 0.0 }
    }

    fn plot_w(&self) -> f64 {
        (self.w - AXIS_W).max(1.0)
    }
}

struct Frame {
    lo: f64,
    hi: f64,
    top: f64,
    bottom: f64,
    vmax: f64,
    vol_top: f64,
    step: f64,
    start: f64,
}

impl Frame {
    fn y(&self, p: f64) -> f64 {
        self.top + (self.hi - p) / (self.hi - self.lo) * (self.bottom - self.top)
    }
    fn x(&self, i: usize) -> f64 {
        (i as f64 - self.start + 0.5) * self.step
    }
    fn vy(&self, v: f64, base: f64) -> f64 {
        base - v / self.vmax * (base - self.vol_top)
    }
}

/// Renders `cs` (oldest first) into `out` as a complete `<svg>` element.
pub fn render(cs: &[Candle], v: &View, out: &mut String) {
    let pw = v.plot_w();
    let base = v.h - AXIS_H;
    out.push_str(r#"<svg xmlns="http://www.w3.org/2000/svg" class="ch" viewBox="0 0 "#);
    fmt::coord(out, v.w);
    out.push(' ');
    fmt::coord(out, v.h);
    out.push_str(r#"" role="img" aria-label="Price chart">"#);

    let i0 = v.start.floor().max(0.0) as usize;
    let i1 = ((v.start + v.count).ceil().max(0.0) as usize).min(cs.len());
    let vis = cs.get(i0..i1.max(i0)).unwrap_or(&[]);
    if vis.is_empty() {
        text(out, "a", v.w / 2.0, v.h / 2.0, "middle", |o| o.push_str("No trades in this range"));
        out.push_str("</svg>");
        return;
    }

    let (mut lo, mut hi, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY, 0.0f64);
    for c in vis {
        lo = lo.min(c.l);
        hi = hi.max(c.h);
        vmax = vmax.max(c.v);
    }
    if hi <= lo {
        let m = if hi > 0.0 { hi * 0.01 } else { 1.0 };
        lo -= m;
        hi += m;
    }
    let pad = (hi - lo) * 0.08;
    let bottom = base - (base - LEGEND_H) * VOLUME;
    let f = Frame {
        lo: lo - pad,
        hi: hi + pad,
        top: LEGEND_H,
        bottom,
        vmax: if vmax > 0.0 { vmax } else { 1.0 },
        vol_top: base - (base - LEGEND_H) * VOLUME * 0.9,
        step: pw / v.count,
        start: v.start,
    };

    // Grid and price labels.
    let tick = nice((f.hi - f.lo) / ((bottom - LEGEND_H) / 56.0).max(1.0));
    let sig = sig_digits(f.hi, tick);
    let mut grid = String::new();
    let mut labels = String::new();
    let mut p = (f.lo / tick).ceil() * tick;
    while p <= f.hi {
        let y = f.y(p);
        if y > LEGEND_H + 6.0 && y < bottom - 4.0 {
            path_h(&mut grid, y, pw);
            text(&mut labels, "", pw + 8.0, y + 4.0, "start", |o| fmt::price_sig(o, p, sig));
        }
        p += tick;
    }
    let tstep = STEPS.iter().copied().find(|s| s / v.res * f.step >= 84.0).unwrap_or(STEPS[STEPS.len() - 1]);
    let mut prev = f64::NAN;
    let mut last_x = f64::NEG_INFINITY;
    for (k, c) in vis.iter().enumerate() {
        let lt = c.t + v.tz;
        let bucket = (lt / tstep).floor();
        if bucket != prev && !prev.is_nan() {
            let x = f.x(i0 + k);
            // Candles are spaced by index, so gaps in trading bunch boundaries together.
            if x > 24.0 && x < pw - 24.0 && x - last_x >= 76.0 {
                last_x = x;
                out_v(&mut grid, x, base);
                // Label the boundary (16:45), not the first candle after a gap (16:46).
                let lt = (bucket * tstep) as i64;
                text(&mut labels, "", x, v.h - 7.0, "middle", |o| {
                    if tstep >= 86_400.0 || lt.rem_euclid(86_400) == 0 {
                        fmt::date(o, lt)
                    } else {
                        fmt::clock(o, lt)
                    }
                });
            }
        }
        prev = bucket;
    }
    path(out, "g", &grid);

    // Volume, wicks and bodies: one path per color keeps the DOM tiny.
    let bw = (f.step * 0.72).max(1.0);
    let (mut up, mut down, mut vup, mut vdown) = (String::new(), String::new(), String::new(), String::new());
    for (k, c) in vis.iter().enumerate() {
        let x = f.x(i0 + k);
        let rising = c.c >= c.o;
        let (body, vol) = if rising { (&mut up, &mut vup) } else { (&mut down, &mut vdown) };
        if c.v > 0.0 {
            rect(vol, x - bw / 2.0, f.vy(c.v, base), bw, base - f.vy(c.v, base));
        }
        let (yh, yl) = (f.y(c.h), f.y(c.l));
        rect(body, x - 0.5, yh, 1.0, (yl - yh).max(1.0));
        let (y0, y1) = (f.y(c.o.max(c.c)), f.y(c.o.min(c.c)));
        rect(body, x - bw / 2.0, y0, bw, (y1 - y0).max(1.0));
    }
    path(out, "vu", &vup);
    path(out, "vd", &vdown);
    path(out, "u", &up);
    path(out, "d", &down);
    out.push_str(r#"<g class="a">"#);
    out.push_str(&labels);
    out.push_str("</g>");

    // Last price line and tag.
    if let Some(last) = cs.last() {
        let y = f.y(last.c);
        if y > LEGEND_H && y < bottom {
            let cls = if last.c >= last.o { "u" } else { "d" };
            out.push_str(r#"<path class="l "#);
            out.push_str(cls);
            out.push_str(r#"" d=""#);
            path_h(out, y, pw);
            out.push_str(r#""/>"#);
            tag(out, cls, pw, y, |o| fmt::price_sig(o, last.c, sig.max(4)));
        }
    }

    // Crosshair, or the latest candle when the pointer is away.
    let mut shown = cs.len() - 1;
    if let Some((cx, cy)) = v.cursor.filter(|&(cx, cy)| cx >= 0.0 && cx < pw && cy >= 0.0 && cy < base) {
        let i = (cx / f.step + v.start).floor();
        if i >= 0.0 && (i as usize) < cs.len() {
            shown = i as usize;
        }
        let mut d = String::new();
        out_v(&mut d, f.x(shown), base);
        path_h(&mut d, cy, pw);
        path(out, "x", &d);
        if cy < bottom {
            let p = f.hi - (cy - f.top) / (f.bottom - f.top) * (f.hi - f.lo);
            tag(out, "t", pw, cy, |o| fmt::price_sig(o, p, sig.max(4)));
        }
        let t = (cs[shown].t + v.tz) as i64;
        let w = 12.0 * CHAR_W;
        let x = (f.x(shown) - w / 2.0).min(pw - w).max(0.0);
        out.push_str(r#"<rect class="t" x=""#);
        fmt::coord(out, x);
        out.push_str(r#"" y=""#);
        fmt::coord(out, base);
        out.push_str(r#"" width=""#);
        fmt::coord(out, w);
        out.push_str(r#"" height=""#);
        fmt::coord(out, AXIS_H);
        out.push_str(r#""/>"#);
        text(out, "tc", x + w / 2.0, v.h - 7.0, "middle", |o| fmt::date_clock(o, t));
    }
    legend(out, &cs[shown]);
    out.push_str("</svg>");
}

fn legend(out: &mut String, c: &Candle) {
    let change = if c.o != 0.0 { c.c / c.o - 1.0 } else { 0.0 };
    let cls = if c.c >= c.o { "u" } else { "d" };
    out.push_str(r#"<text class="a" x="6" y="17">"#);
    for (name, val) in [("O", c.o), ("H", c.h), ("L", c.l), ("C", c.c)] {
        out.push_str(name);
        out.push_str(r#" <tspan class=""#);
        out.push_str(cls);
        out.push_str(r#"">"#);
        fmt::price(out, val);
        out.push_str("</tspan>  ");
    }
    out.push_str(r#"<tspan class=""#);
    out.push_str(cls);
    out.push_str(r#"">"#);
    fmt::pct(out, change);
    out.push_str("</tspan>  Vol $");
    fmt::compact(out, c.v);
    out.push_str("</text>");
}

/// A filled label box on the price axis.
fn tag(out: &mut String, cls: &str, pw: f64, y: f64, f: impl FnOnce(&mut String)) {
    let mut label = String::new();
    f(&mut label);
    out.push_str(r#"<rect class=""#);
    out.push_str(cls);
    out.push_str(r#"" x=""#);
    fmt::coord(out, pw + 1.0);
    out.push_str(r#"" y=""#);
    fmt::coord(out, y - 9.0);
    out.push_str(r#"" width=""#);
    fmt::coord(out, AXIS_W - 2.0);
    out.push_str(r#"" height="18" rx="3"/>"#);
    // Crosshair tags ("t") sit on the foreground color; price tags on up/down colors.
    let text_cls = if cls == "t" { "tc" } else { "tt" };
    text(out, text_cls, pw + 8.0, y + 4.0, "start", |o| o.push_str(&label));
}

fn text(out: &mut String, cls: &str, x: f64, y: f64, anchor: &str, f: impl FnOnce(&mut String)) {
    out.push_str("<text");
    if !cls.is_empty() {
        out.push_str(r#" class=""#);
        out.push_str(cls);
        out.push('"');
    }
    out.push_str(r#" x=""#);
    fmt::coord(out, x);
    out.push_str(r#"" y=""#);
    fmt::coord(out, y);
    out.push_str(r#"" text-anchor=""#);
    out.push_str(anchor);
    out.push_str(r#"">"#);
    f(out);
    out.push_str("</text>");
}

fn path(out: &mut String, cls: &str, d: &str) {
    if d.is_empty() {
        return;
    }
    out.push_str(r#"<path class=""#);
    out.push_str(cls);
    out.push_str(r#"" d=""#);
    out.push_str(d);
    out.push_str(r#""/>"#);
}

fn path_h(d: &mut String, y: f64, w: f64) {
    d.push_str("M0 ");
    fmt::coord(d, y);
    d.push('H');
    fmt::coord(d, w);
}

fn out_v(d: &mut String, x: f64, h: f64) {
    d.push('M');
    fmt::coord(d, x);
    d.push_str(" 0V");
    fmt::coord(d, h);
}

fn rect(d: &mut String, x: f64, y: f64, w: f64, h: f64) {
    d.push('M');
    fmt::coord(d, x);
    d.push(' ');
    fmt::coord(d, y);
    d.push('h');
    fmt::coord(d, w);
    d.push('v');
    fmt::coord(d, h);
    d.push('h');
    fmt::coord(d, -w);
    d.push('z');
}

/// 1, 2 or 5 times a power of ten, at least `raw`.
fn nice(raw: f64) -> f64 {
    let p = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * p).find(|&s| s >= raw).unwrap_or(10.0 * p)
}

/// Significant digits needed so that labels `tick` apart stay distinct.
fn sig_digits(hi: f64, tick: f64) -> i32 {
    ((hi.abs().log10().floor() - tick.log10().floor()) as i32 + 1).max(2).min(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(n: usize) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let p = 1.0 + (i as f64 * 0.3).sin() * 0.1;
                Candle { t: 1_794_992_700.0 + i as f64 * 60.0, o: p, h: p * 1.02, l: p * 0.98, c: p * 1.01, v: 100.0 }
            })
            .collect()
    }

    #[test]
    fn renders_paths_and_labels() {
        let cs = series(300);
        let mut out = String::new();
        let mut v = View::latest(cs.len(), 960.0, 420.0, 60.0);
        v.cursor = Some((400.0, 200.0));
        render(&cs, &v, &mut out);
        assert!(out.starts_with("<svg") && out.ends_with("</svg>"));
        assert!(out.contains(r#"class="u""#) && out.contains(r#"class="vu""#));
        assert!(out.contains(r#"class="x""#), "crosshair drawn");
        assert!(out.contains("Vol $100"));
        assert!(out.len() < 40_000, "svg stays small: {}", out.len());
    }

    #[test]
    fn empty_and_flat() {
        let mut out = String::new();
        render(&[], &View::latest(0, 600.0, 300.0, 60.0), &mut out);
        assert!(out.contains("No trades"));
        let flat = [Candle { t: 0.0, o: 1.0, h: 1.0, l: 1.0, c: 1.0, v: 0.0 }];
        out.clear();
        render(&flat, &View::latest(1, 600.0, 300.0, 60.0), &mut out);
        assert!(!out.contains("NaN") && !out.contains("inf"));
    }

    #[test]
    fn nice_steps() {
        assert_eq!(nice(0.7), 1.0);
        assert_eq!(nice(1.2), 2.0);
        assert_eq!(nice(31.0), 50.0);
    }
}
