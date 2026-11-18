//! Share images: the same chart as the page, rasterized with resvg.

use crate::model::RES;
use crate::pages::{Esc, LOGO, PALETTE, change, chart_css, chart_data, default_res, metrics, price_parts};
use crate::state::State;
use crate::store::Store;
use anyhow::{Context, Result};
use chart::fmt;
use resvg::{tiny_skia, usvg};
use std::fmt::Write;
use std::sync::{Arc, LazyLock};

const FONT: &str = "Inter, DejaVu Sans, Arial, sans-serif";

static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

fn dark(name: &str) -> String {
    PALETTE.iter().find(|(n, ..)| *n == name).map(|(.., d)| d.to_string()).unwrap_or_default()
}

pub fn png(svg: &str) -> Result<Vec<u8>> {
    let opt = usvg::Options { fontdb: FONTS.clone(), font_family: "Inter".into(), ..Default::default() };
    let tree = usvg::Tree::from_str(svg, &opt).context("parse svg")?;
    let size = tree.size().to_int_size();
    let mut pix = tiny_skia::Pixmap::new(size.width(), size.height()).context("pixmap")?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pix.as_mut());
    pix.encode_png().context("encode png")
}

fn frame(w: u32, h: u32, body: &str) -> String {
    let css = chart_css(dark);
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="{FONT}"><style>{css}.fg{{fill:{fg}}}.mu{{fill:{mu}}}.up{{fill:{up}}}.dn{{fill:{dn}}}</style><rect width="{w}" height="{h}" fill="{bg}"/>{body}</svg>"#,
        fg = dark("fg"),
        mu = dark("muted"),
        up = dark("up"),
        dn = dark("down"),
        bg = dark("bg"),
    )
}

fn logo(x: u32, y: u32, size: u32) -> String {
    LOGO.replacen("<svg ", &format!(r#"<svg x="{x}" y="{y}" width="{size}" height="{size}" "#), 1)
}

/// A pair card: name, price, change, key numbers and the 1 h chart.
/// `w`×`h` is 1200×630 for OG and 1200×800 (3:2) for Farcaster.
pub fn pair_card(st: &State, store: &Store, p: u32, w: u32, h: u32) -> String {
    let pool = &st.pools[p as usize];
    let sym = |t: u32| Esc(&st.tokens[t as usize].symbol).to_string();
    let m = metrics(st, p);
    let ch = change(&m, 3);
    use fmt::s;
    let mut b = String::new();
    let _ = write!(
        b,
        r#"{logo}<text x="128" y="96" font-size="54" font-weight="700" class="fg">{base}<tspan class="mu">/{quote}</tspan></text><text x="{right}" y="92" font-size="28" text-anchor="end" class="mu">plainscreener.com</text><text x="56" y="196" font-size="72" font-weight="700" class="fg">{price}</text><text x="56" y="250" font-size="30" class="mu">{dex} on Base · 24h <tspan class="{cls}">{change}</tspan></text>"#,
        logo = logo(56, 44, 56),
        base = sym(pool.base()),
        quote = sym(pool.quote()),
        right = w - 56,
        price = match price_parts(st, p, &m) {
            (n, Some(unit)) => format!(r#"{n}<tspan dx="12" font-size="32" font-weight="400" class="mu">{}</tspan>"#, Esc(unit)),
            (n, None) => n,
        },
        dex = pool.dex(),
        cls = if ch >= 0.0 { "up" } else { "dn" },
        change = s(|o| fmt::pct(o, ch)),
    );
    for (i, (label, val)) in [
        ("Liquidity", s(|o| fmt::usd_compact(o, m.liq))),
        ("Volume 24h", s(|o| fmt::usd_compact(o, m.w[3].vol))),
        ("FDV", s(|o| fmt::usd_compact(o, m.fdv))),
    ]
    .iter()
    .enumerate()
    {
        let x = w - 56 - (2 - i as u32) * 230;
        let _ = write!(
            b,
            r#"<text x="{x}" y="166" font-size="24" text-anchor="end" class="mu">{label}</text><text x="{x}" y="206" font-size="34" font-weight="700" text-anchor="end" class="fg">{val}</text>"#
        );
    }
    // The chart is drawn at 1/1.6 size and scaled up, so its labels read at OG size.
    let (cx, cy) = (24u32, 280u32);
    let (cw, chh) = (w - 48, h - cy - 24);
    let scale = 1.6;
    let ri = default_res(st, p, 3);
    let candles = chart_data(st, store, p, ri, u32::MAX, 300);
    let (vw, vh) = (cw as f64 / scale, chh as f64 / scale);
    let mut view = chart::View::latest(candles.len(), vw, vh, RES[ri] as f64);
    view.count = view.count.min(candles.len().max(30) as f64 + 2.0);
    view.start = candles.len() as f64 + 2.0 - view.count;
    let svg = s(|o| chart::render(&candles, &view, o));
    b.push_str(&svg.replacen(
        r#"<svg xmlns="http://www.w3.org/2000/svg""#,
        &format!(r#"<svg x="{cx}" y="{cy}" width="{cw}" height="{chh}""#),
        1,
    ));
    frame(w, h, &b)
}

/// The site card used by the home page.
pub fn site_card() -> String {
    let body = format!(
        r#"{logo}<text x="600" y="380" font-size="76" text-anchor="middle" class="fg">plain<tspan font-weight="800">screener</tspan></text><text x="600" y="450" font-size="34" text-anchor="middle" class="mu">Live prices and charts for every token on Base</text>"#,
        logo = logo(540, 160, 120),
    );
    frame(1200, 630, &body)
}

/// The square app icon (also the mini app splash image).
pub fn icon() -> String {
    LOGO.replacen("<svg ", r#"<svg width="512" height="512" "#, 1)
}
