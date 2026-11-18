//! Browser entry points. JS writes candles into the buffer from `data`, calls `draw`,
//! and reads `len()` UTF-8 bytes of SVG at the returned pointer.

use crate::{AXIS_W, Candle, View, render};
use std::cell::UnsafeCell;

struct State {
    candles: Vec<Candle>,
    svg: String,
    view: View,
}

struct Global(UnsafeCell<State>);
// SAFETY: wasm32-unknown-unknown without the atomics feature has a single thread.
unsafe impl Sync for Global {}

static S: Global = Global(UnsafeCell::new(State {
    candles: Vec::new(),
    svg: String::new(),
    view: View { w: 0.0, h: 0.0, res: 60.0, start: 0.0, count: 1.0, cursor: None, tz: 0.0 },
}));

fn state() -> &'static mut State {
    // SAFETY: single-threaded, and no export holds a reference across calls.
    unsafe { &mut *S.0.get() }
}

/// Resizes the candle buffer to `n` candles and returns it as `[t, o, h, l, c, v] * n` f64s.
#[unsafe(no_mangle)]
pub extern "C" fn data(n: usize) -> *mut f64 {
    let s = state();
    s.candles.resize(n, Candle::default());
    s.candles.as_mut_ptr().cast()
}

/// Renders the buffer. A NaN `start` picks the default (latest) view; negative `cx`
/// hides the crosshair. Returns a pointer to the SVG; its length is `len()`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn draw(w: f64, h: f64, res: f64, start: f64, count: f64, cx: f64, cy: f64, tz: f64) -> *const u8 {
    let s = state();
    let mut v = View::latest(s.candles.len(), w, h, res);
    if !start.is_nan() {
        v.start = start;
        v.count = count.max(5.0);
    }
    v.cursor = (cx >= 0.0).then_some((cx, cy));
    v.tz = tz;
    s.svg.clear();
    render(&s.candles, &v, &mut s.svg);
    s.view = v;
    s.svg.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn len() -> usize {
    state().svg.len()
}

/// The view used by the last `draw`, so JS can pan and zoom from it.
#[unsafe(no_mangle)]
pub extern "C" fn start() -> f64 {
    state().view.start
}

#[unsafe(no_mangle)]
pub extern "C" fn count() -> f64 {
    state().view.count
}

#[unsafe(no_mangle)]
pub extern "C" fn plot_w(w: f64) -> f64 {
    w - AXIS_W
}
