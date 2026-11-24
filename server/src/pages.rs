//! Server-rendered HTML. Every number on a page is formatted here (or in `chart::fmt`),
//! and live updates reuse the same functions, so pages and updates always agree.

use crate::App;
use crate::abi::hex;
use crate::lists::{self, Filter, Tab};
use crate::model::{Pool, RES, RES_NAMES, Trade, TradeKind, Window};
use crate::state::{NONE, State};
use crate::store::Store;
use chart::fmt;
use std::fmt::{Display, Write};

pub const SITE: &str = "plainscreener";
pub const REPO: &str = "https://github.com/backmeupplz/plainscreener";
const EXPLORER: &str = "https://basescan.org";
/// Candles sent with a page and per API request.
pub const CHART_CANDLES: usize = 300;

/// Colors: (name, light, dark). The page gets them as CSS variables, OG images inline.
/// Near-black surfaces, hairline borders, one blue accent.
pub const PALETTE: [(&str, &str, &str); 29] = [
    // Avatar background and letter lightness (hue comes from the token address).
    ("avbg", "90%", "22%"),
    ("avfg", "30%", "82%"),
    ("bg", "#fafafa", "#000000"),
    ("card", "#ffffff", "#0d0d0d"),
    ("raise", "#f4f4f5", "#1a1a1a"),
    ("fg", "#0a0a0a", "#f5f5f5"),
    ("fg2", "#1f4fd1", "#ffffff"),
    ("muted", "#666666", "#a3a3a3"),
    ("line", "rgba(0,0,0,0.08)", "rgba(255,255,255,0.08)"),
    ("line2", "rgba(0,0,0,0.05)", "rgba(255,255,255,0.05)"),
    ("line3", "rgba(0,0,0,0.2)", "rgba(255,255,255,0.2)"),
    ("grid", "rgba(0,0,0,0.05)", "rgba(255,255,255,0.05)"),
    ("hover", "rgba(0,0,0,0.025)", "rgba(255,255,255,0.03)"),
    ("nav", "rgba(250,250,250,0.8)", "rgba(0,0,0,0.75)"),
    ("up", "#0a7a3d", "#3ecf8e"),
    ("down", "#d0283c", "#ff5d6c"),
    ("upbg", "rgba(10,122,61,0.1)", "rgba(62,207,142,0.12)"),
    ("downbg", "rgba(208,40,60,0.1)", "rgba(255,93,108,0.12)"),
    ("accent", "#2d63ea", "#2d63ea"),
    ("accent2", "#1f53d6", "#3d72f5"),
    ("ring", "rgba(45,99,234,0.2)", "rgba(45,99,234,0.35)"),
    ("shadow", "0 1px 2px rgba(0,0,0,0.04)", "0 12px 32px -16px rgba(0,0,0,0.65)"),
    ("white", "#ffffff", "#ffffff"),
    // Moving averages on the chart.
    ("ma1", "#c2410c", "#f5a524"),
    ("ma2", "#7c3aed", "#a78bfa"),
    ("vwap", "#0e7490", "#22d3ee"),
    ("rsi", "#be185d", "#f472b6"),
    // Which icon the theme button shows.
    ("sun", "none", "block"),
    ("moon", "block", "none"),
];

/// Chart styles, with `{name}` standing for a palette color.
const CHART_CSS: &str = ".ch .g{stroke:{grid};fill:none}.ch .u{fill:{up}}.ch .d{fill:{down}}\
.ch .vu{fill:{up};opacity:.28}.ch .vd{fill:{down};opacity:.28}.ch text{font-size:12px;font-family:system-ui,Inter,sans-serif}\
.ch .a{fill:{muted}}.ch .l{fill:none;stroke-dasharray:2 3}.ch .l.u{stroke:{up}}.ch .l.d{stroke:{down}}\
.ch .x{fill:none;stroke:{muted};stroke-dasharray:3 3}.ch .ln{fill:none;stroke:{accent};stroke-width:1.6;stroke-linejoin:round}\
.ch .m1,.ch .m2{fill:none;stroke-width:1.3}.ch .m1{stroke:{ma1}}.ch .m2{stroke:{ma2}}\
.ch .vw{fill:none;stroke:{vwap};stroke-width:1.3}.ch .rsi{fill:none;stroke:{rsi};stroke-width:1.3}\
.ch .rg{fill:none;stroke:{muted};stroke-dasharray:2 4;opacity:.6}\
.ch .mku{fill:{up};stroke:{bg}}.ch .mkd{fill:{down};stroke:{bg}}.ch .t{fill:{fg}}.ch .tc{fill:{bg}}.ch .tt{fill:{white}}";

/// Chart CSS with colors from `value(name)`.
pub fn chart_css(value: impl Fn(&str) -> String) -> String {
    PALETTE.iter().fold(CHART_CSS.to_string(), |css, (name, ..)| css.replace(&format!("{{{name}}}"), &value(name)))
}

/// The whole page stylesheet, inlined into every page. The system theme applies unless
/// the theme button set `data-theme` on `<html>`.
pub fn page_css() -> String {
    let vars = |dark: bool| {
        PALETTE.iter().map(|(n, l, d)| format!("--{n}:{}", if dark { d } else { l })).collect::<Vec<_>>().join(";")
    };
    let (light, dark) = (vars(false), vars(true));
    format!(
        ":root{{{light};color-scheme:light}}@media(prefers-color-scheme:dark){{:root:not([data-theme=light]){{{dark};color-scheme:dark}}}}:root[data-theme=dark]{{{dark};color-scheme:dark}}{}{}",
        include_str!("../../web/style.css"),
        chart_css(|n| format!("var(--{n})")),
    )
}

/// Runs before first paint: applies the theme the visitor picked (pages are cached for
/// everyone, so the server can't).
const THEME_JS: &str = r##"try{var t=localStorage.getItem("theme");if(t){document.documentElement.dataset.theme=t;document.querySelectorAll("meta[name=theme-color]").forEach(function(m){m.content=t=="dark"?"#000000":"#fafafa"})}}catch(e){}"##;

/// Chrome prerenders a pair page while the pointer rests on its link, so it opens instantly.
const SPECULATION: &str = r#"{"prerender":[{"where":{"href_matches":"/base/*"},"eagerness":"moderate"}]}"#;

/// The page CSP: our own scripts plus the two inline ones above, by hash.
pub static CSP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use base64::Engine;
    let hash = |js: &str| {
        base64::engine::general_purpose::STANDARD.encode(ring::digest::digest(&ring::digest::SHA256, js.as_bytes()))
    };
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' 'sha256-{}' 'sha256-{}' https://lumen.borodutch.com; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self' https://lumen.borodutch.com; base-uri 'none'; form-action 'self'",
        hash(THEME_JS),
        hash(SPECULATION)
    )
});

const THEME_BUTTON: &str = r##"<button type="button" class="ic theme" aria-label="Switch between light and dark theme" title="Theme"><svg class="i sun" aria-hidden="true"><use href="#i-sun"/></svg><svg class="i moon" aria-hidden="true"><use href="#i-moon"/></svg></button>"##;

/// The icon sprite (Lucide and Phosphor paths), inlined once per page for `icon`.
const ICONS: &str = include_str!("../../web/icons.svg");

pub const LOGO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" aria-hidden="true"><rect x=".5" y=".5" width="31" height="31" rx="8" fill="#0d0d0d" stroke="#ffffff" stroke-opacity=".14"/><path d="M9 7v18M16 10v16M23 5v16" stroke="#a3a3a3" stroke-width="1.6"/><rect x="7" y="11" width="4" height="9" rx="1" fill="#3ecf8e"/><rect x="14" y="14" width="4" height="8" rx="1" fill="#ff5d6c"/><rect x="21" y="8" width="4" height="9" rx="1" fill="#3ecf8e"/></svg>"##;

/// HTML-escapes untrusted text (token names and symbols come from anyone's contract).
pub struct Esc<'a>(pub &'a str);

impl Display for Esc<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for c in self.0.chars() {
            match c {
                '&' => f.write_str("&amp;")?,
                '<' => f.write_str("&lt;")?,
                '>' => f.write_str("&gt;")?,
                '"' => f.write_str("&quot;")?,
                '\'' => f.write_str("&#39;")?,
                c => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// An icon from the sprite (web/icons.svg), sized by CSS.
pub(crate) fn icon(name: &str) -> String {
    format!(r##"<svg class="i" aria-hidden="true"><use href="#i-{name}"/></svg>"##)
}

pub(crate) fn s(f: impl FnOnce(&mut String)) -> String {
    fmt::s(f)
}

fn sym(st: &State, t: u32) -> &str {
    let t = &st.tokens[t as usize];
    if t.symbol.is_empty() { "?" } else { &t.symbol }
}

fn short(a: &[u8]) -> String {
    let h = hex(a);
    format!("{}…{}", &h[..6], &h[h.len() - 4..])
}

pub struct Head {
    pub title: String,
    pub desc: String,
    /// Path of the canonical URL, e.g. "/base/0x…".
    pub path: String,
    pub image: Option<String>,
    /// Farcaster mini app card: (3:2 image, launch URL).
    pub miniapp: Option<(String, String)>,
    /// X player card: a page X frames inside the post (the pair's embed).
    pub player: Option<String>,
    pub embed: bool,
}

/// `head` (the indexed head time) comes from the caller's state guard: taking the state
/// lock again here deadlocks once the indexer queues for a write in between (std's RwLock
/// prefers writers), which hung the site on 2026-11-19.
pub fn layout(app: &App, head: u32, h: &Head, body: &str) -> String {
    let url = format!("{}{}", app.cfg.public_url, h.path);
    let mut meta = String::new();
    let (title, desc) = (Esc(&h.title), Esc(&h.desc));
    let _ = write!(
        meta,
        r#"<meta property="og:type" content="website"><meta property="og:site_name" content="{SITE}"><meta property="og:title" content="{title}"><meta property="og:description" content="{desc}"><meta property="og:url" content="{url}"><meta name="twitter:title" content="{title}"><meta name="twitter:description" content="{desc}">"#
    );
    if let Some(img) = &h.image {
        let _ = write!(
            meta,
            r#"<meta property="og:image" content="{img}"><meta property="og:image:width" content="1200"><meta property="og:image:height" content="630"><meta property="og:image:alt" content="{title}"><meta name="twitter:image" content="{img}">"#
        );
    }
    match &h.player {
        // Playable in the post: X frames the embed; the image is the fallback.
        Some(player) => {
            let _ = write!(
                meta,
                r#"<meta name="twitter:card" content="player"><meta name="twitter:player" content="{player}"><meta name="twitter:player:width" content="640"><meta name="twitter:player:height" content="400">"#
            );
        }
        None if h.image.is_some() => meta.push_str(r#"<meta name="twitter:card" content="summary_large_image">"#),
        None => {}
    }
    if let Some((img, launch)) = &h.miniapp {
        for (name, action) in [("fc:miniapp", "launch_miniapp"), ("fc:frame", "launch_frame")] {
            let card = serde_json::json!({
                "version": "1",
                "imageUrl": img,
                "button": {"title": "Open chart", "action": {
                    "type": action, "name": SITE, "url": launch,
                    "splashImageUrl": format!("{}/icon.png", app.cfg.public_url),
                    "splashBackgroundColor": "#000000"
                }}
            });
            let _ = write!(meta, r#"<meta name="{name}" content="{}">"#, Esc(&card.to_string()));
        }
    }
    let header = if h.embed {
        String::new()
    } else {
        format!(
            r#"<header class="top"><div class="wrap"><a class="logo" href="/" aria-label="{SITE} home">{LOGO}<span>plain<b>screener</b></span></a>{search}<nav class="hn" aria-label="Site"><a class="hl" href="/api">API</a><a class="ic" href="{REPO}" aria-label="Source code on GitHub" title="GitHub">{gh}</a>{THEME_BUTTON}</nav></div></header>"#,
            search = SEARCH_FORM,
            gh = icon("github"),
        )
    };
    // While the indexer replays history, say so instead of passing old data off as live.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let header = if !h.embed && now > head as u64 + 600 {
        let at = if head == 0 { "the start".to_string() } else { s(|o| fmt::date_year(o, head as i64)) };
        format!(
            r#"{header}<div class="wrap"><p class="note">Still replaying Base history, now at {at}. Prices and lists here are from that day until the indexer reaches the present.</p></div>"#
        )
    } else {
        header
    };
    let footer = if h.embed {
        String::new()
    } else {
        format!(
            r#"<footer><div class="wrap">Every Base DEX pool, indexed straight from the chain. <a href="{REPO}">Open source</a> · <a href="/api">Free API</a>. Not financial advice.</div></footer>"#
        )
    };
    format!(
        r##"<!doctype html><html lang="en"{cls}><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><meta name="description" content="{desc}"><link rel="canonical" href="{url}">{meta}<link rel="icon" href="/favicon.svg" type="image/svg+xml"><link rel="apple-touch-icon" href="/icon.png"><meta name="theme-color" content="#000000" media="(prefers-color-scheme: dark)"><meta name="theme-color" content="#fafafa" media="(prefers-color-scheme: light)"><script>{THEME_JS}</script><style>{css}</style><script defer src="https://lumen.borodutch.com/l.js"></script></head><body>{ICONS}{header}{body}{footer}<script type="module" src="{js}"></script><script type="speculationrules">{SPECULATION}</script></body></html>"##,
        cls = if h.embed { r#" class="embed""# } else { "" },
        css = app.css,
        js = app.js_url,
    )
}

/// A pair page's title, which live updates keep current.
pub fn pair_title(st: &State, p: u32) -> String {
    let pool = &st.pools[p as usize];
    let m = metrics(st, p);
    let (bs, qs) = (sym(st, pool.base()), sym(st, pool.quote()));
    format!("{bs} {} {} | {bs}/{qs} on Base | {SITE}", price_label(st, p, &m), s(|o| fmt::pct(o, change(&m, 3))))
}

/// Everything shown about a pool right now.
pub struct Metrics {
    pub price: f64,
    pub native: f64,
    pub liq: f64,
    pub fdv: f64,
    /// 5 m, 1 h, 6 h and 24 h.
    pub w: [Window; 4],
}

pub const WINDOWS: [(&str, u32); 4] = [("5m", 300), ("1h", 3600), ("6h", 21_600), ("24h", 86_400)];

pub fn metrics(st: &State, p: u32) -> Metrics {
    let pool = &st.pools[p as usize];
    let base = &st.tokens[pool.base() as usize];
    let stats = st.stats.get(&p);
    let w = WINDOWS.map(|(_, secs)| stats.map(|s| s.window(st.head_ts, secs)).unwrap_or_default());
    let price = if pool.price_usd.is_finite() { pool.price_usd } else { base.usd };
    Metrics { price, native: pool.price_native, liq: st.liq(p), fdv: base.supply * price, w }
}

/// The price in USD, or in the quote token (number, symbol) before that has a USD price.
pub fn price_parts<'a>(st: &'a State, p: u32, m: &Metrics) -> (String, Option<&'a str>) {
    if m.price.is_finite() {
        (s(|o| fmt::usd(o, m.price)), None)
    } else {
        (s(|o| fmt::price(o, m.native)), Some(sym(st, st.pools[p as usize].quote())))
    }
}

pub fn price_label(st: &State, p: u32, m: &Metrics) -> String {
    match price_parts(st, p, m) {
        (n, Some(unit)) => format!("{n} {unit}"),
        (n, None) => n,
    }
}

/// The largest candle size up to `RES[max]` that still gives a young pool a few dozen candles.
pub fn default_res(st: &State, p: u32, max: usize) -> usize {
    let age = st.head_ts.saturating_sub(st.pools[p as usize].created_ts);
    (0..=max).rev().find(|&ri| age / RES[ri] >= 48).unwrap_or(0)
}

pub fn change(m: &Metrics, i: usize) -> f64 {
    let open = m.w[i].open;
    if open > 0.0 && m.price.is_finite() { m.price / open - 1.0 } else { f64::NAN }
}

pub(crate) fn dir(x: f64) -> &'static str {
    if x > 0.0 {
        "u"
    } else if x < 0.0 {
        "d"
    } else {
        "m"
    }
}

/// Trades looked at for makers and top traders (newest first).
const TRADER_SCAN: usize = 100_000;

/// Unique wallets that traded in each window (`WINDOWS`): (makers, buyers, sellers), from
/// the pool's stored trades. Cached for 15 s per pool: a busy pool has many thousands.
pub fn makers(store: &Store, p: u32, now: u32) -> [(u32, u32, u32); 4] {
    use std::collections::{HashMap, HashSet};
    use std::sync::{LazyLock, Mutex};
    type Cache = HashMap<u32, (std::time::Instant, [(u32, u32, u32); 4])>;
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
    if let Some((at, m)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&p)
        && at.elapsed().as_secs() < 15
    {
        return *m;
    }
    let mut sets: [[HashSet<[u8; 20]>; 3]; 4] = Default::default();
    let from = now.saturating_sub(WINDOWS[3].1);
    for (_, t) in store.trades(p, (u32::MAX, u32::MAX), TRADER_SCAN) {
        if t.ts < from {
            break;
        }
        if t.kind != TradeKind::Swap {
            continue;
        }
        for (w, (_, secs)) in WINDOWS.iter().enumerate() {
            if t.ts + secs >= now {
                sets[w][0].insert(t.maker);
                sets[w][if t.buy { 1 } else { 2 }].insert(t.maker);
            }
        }
    }
    let m = sets.map(|[a, b, s]| (a.len() as u32, b.len() as u32, s.len() as u32));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() > 10_000 {
        cache.clear();
    }
    cache.insert(p, (std::time::Instant::now(), m));
    m
}

/// Live fields plus makers per window (`k5m` …), which need the store.
/// Live fields plus wallets per window (`k5m` …, buyers `kb5m`, sellers `ks5m`), which
/// need the store, and the buy shares of trades and wallets for the bars (`rb5m`, `rk5m`).
pub fn fields_full(st: &State, store: &Store, p: u32) -> Vec<(String, String, &'static str)> {
    let mut fs = fields(st, p);
    let m = metrics(st, p);
    let share = |a: u32, b: u32| {
        if a + b == 0 { "50%".to_string() } else { format!("{:.1}%", a as f64 * 100.0 / (a + b) as f64) }
    };
    for (i, ((name, _), (makers, buyers, sellers))) in WINDOWS.iter().zip(makers(store, p, st.head_ts)).enumerate() {
        fs.push((format!("k{name}"), s(|o| fmt::uint(o, makers as u64)), ""));
        fs.push((format!("kb{name}"), s(|o| fmt::uint(o, buyers as u64)), ""));
        fs.push((format!("ks{name}"), s(|o| fmt::uint(o, sellers as u64)), ""));
        fs.push((format!("rb{name}"), share(m.w[i].buys, m.w[i].sells), "bar"));
        fs.push((format!("rk{name}"), share(buyers, sellers), "bar"));
    }
    fs
}

/// Trading results of one wallet in a pool: what it paid and got, both sides.
#[derive(Default, Clone, Copy)]
struct Wallet {
    bought: f64,
    sold: f64,
    got: f64,
    gave: f64,
    buys: u32,
    sells: u32,
}

impl Wallet {
    /// Profit on what it bought here: sales of those tokens at their average sell price,
    /// plus any still held at `price`, minus what it paid. Tokens sold beyond what it
    /// bought here came from elsewhere, so they don't count. None without buys.
    fn pnl(&self, price: f64) -> Option<f64> {
        if self.got <= 0.0 || self.bought <= 0.0 {
            return None;
        }
        let avg_buy = self.bought / self.got;
        let matched = self.got.min(self.gave);
        let realized = if self.gave > 0.0 { matched * (self.sold / self.gave - avg_buy) } else { 0.0 };
        let open = (self.got - self.gave).max(0.0);
        let unrealized = if price.is_finite() { open * (price - avg_buy) } else { 0.0 };
        Some(realized + unrealized)
    }
}

/// The most profitable wallets among a pool's last `TRADER_SCAN` trades, as table rows
/// (see `Wallet::pnl`).
pub fn top_traders(st: &State, store: &Store, p: u32) -> String {
    let pool = &st.pools[p as usize];
    let mut by: std::collections::HashMap<[u8; 20], Wallet> = Default::default();
    for (_, t) in store.trades(p, (u32::MAX, u32::MAX), TRADER_SCAN) {
        if t.kind != TradeKind::Swap || !t.usd.is_finite() {
            continue;
        }
        let w = by.entry(t.maker).or_default();
        if t.buy {
            (w.bought, w.got, w.buys) = (w.bought + t.usd as f64, w.got + t.base, w.buys + 1);
        } else {
            (w.sold, w.gave, w.sells) = (w.sold + t.usd as f64, w.gave + t.base, w.sells + 1);
        }
    }
    let mut ws: Vec<([u8; 20], Wallet, f64)> =
        by.into_iter().filter_map(|(a, w)| Some((a, w, w.pnl(pool.price_usd)?))).collect();
    ws.sort_unstable_by(|a, b| b.2.total_cmp(&a.2));
    if ws.is_empty() {
        return r#"<tr><td colspan="6" class="empty">No wallet has bought here yet.</td></tr>"#.into();
    }
    let bs = Esc(sym(st, pool.base())).to_string();
    ws.iter()
        .take(50)
        .enumerate()
        .map(|(i, (a, w, pnl))| {
            let amount = |usd: f64, base: f64| {
                s(|o| {
                    fmt::usd_compact(o, usd);
                    o.push_str(r#"<small class="m">"#);
                    fmt::compact(o, base);
                    let _ = write!(o, " {bs}</small>");
                })
            };
            let cls = if *pnl >= 0.0 { "u" } else { "d" };
            format!(
                r#"<tr><td class="l m">{n}</td><td class="l"><button type="button" class="mk mono" data-maker="{maker}" title="Only this wallet's trades">{short}</button></td><td>{bought}</td><td>{sold}</td><td class="{cls}"><b>{pnl}</b><small>{roi}</small></td><td class="hide-s"><span class="u">{buys}</span> <span class="m">/</span> <span class="d">{sells}</span></td></tr>"#,
                n = i + 1,
                maker = hex(a),
                short = short(a),
                bought = amount(w.bought, w.got),
                sold = if w.sells > 0 { amount(w.sold, w.gave) } else { r#"<span class="m">–</span>"#.into() },
                pnl = s(|o| {
                    if *pnl >= 0.0 {
                        o.push('+');
                    }
                    fmt::usd_compact(o, *pnl);
                }),
                roi = s(|o| fmt::pct(o, pnl / w.bought)),
                buys = w.buys,
                sells = w.sells,
            )
        })
        .collect()
}

/// Live fields of a pair page: (id, text, class). Used for the first render and for updates.
pub fn fields(st: &State, p: u32) -> Vec<(String, String, &'static str)> {
    let m = metrics(st, p);
    let pool = &st.pools[p as usize];
    let mut out = vec![
        ("p".into(), s(|o| fmt::usd(o, m.price)), ""),
        (
            "pn".into(),
            s(|o| {
                fmt::price(o, m.native);
                o.push(' ');
                o.push_str(sym(st, pool.quote()));
            }),
            "",
        ),
        ("liq".into(), s(|o| fmt::usd_compact(o, m.liq)), ""),
        ("fdv".into(), s(|o| fmt::usd_compact(o, m.fdv)), ""),
    ];
    for (i, (name, _)) in WINDOWS.iter().enumerate() {
        let c = change(&m, i);
        out.push((format!("c{name}"), s(|o| fmt::pct(o, c)), dir(c)));
        out.push((format!("v{name}"), s(|o| fmt::usd_compact(o, m.w[i].vol)), ""));
        out.push((format!("b{name}"), s(|o| fmt::uint(o, m.w[i].buys as u64)), ""));
        out.push((format!("s{name}"), s(|o| fmt::uint(o, m.w[i].sells as u64)), ""));
    }
    // Amounts without the symbol: their labels name the token.
    let amount = |x: f64| s(|o| fmt::compact(o, x));
    let (bb, qb) = if pool.quote0 { (pool.bal1, pool.bal0) } else { (pool.bal0, pool.bal1) };
    out.push(("sup".into(), amount(st.tokens[pool.base() as usize].supply), ""));
    out.push(("pb".into(), amount(bb), ""));
    out.push(("pq".into(), amount(qb), ""));
    out
}

/// A live field as HTML. `fields` holds plain text (live updates set `textContent`),
/// so it's escaped here: the quote symbol in `pn` is anyone's to choose.
fn field(fs: &[(String, String, &'static str)], id: &str) -> String {
    fs.iter()
        .find(|(i, ..)| i == id)
        .map(|(i, text, cls)| match *cls {
            // A bar's width is its text (app.js does the same on live updates).
            "bar" => format!(r#"<i data-f="{i}" class="bar" style="--r:{0}">{0}</i>"#, Esc(text)),
            _ => format!(r#"<span data-f="{i}" class="{cls}">{}</span>"#, Esc(text)),
        })
        .unwrap_or_default()
}

/// Trades per page (the first page and each "Load more").
pub const TRADES: usize = 50;

/// "Load more" under the trades table, pointing at the oldest trade shown.
fn more_button(page: &[((u32, u32), Trade)]) -> String {
    match page.last() {
        Some(((b, i), _)) if page.len() == TRADES => {
            format!(r#"<button type="button" class="more" id="more" data-before="{b}-{i}">Load more trades</button>"#)
        }
        _ => String::new(),
    }
}

/// Trades a wallet filter looks through per page.
const MAKER_SCAN: usize = 20_000;

/// A page of trade rows for "Load more" or a wallet filter: `{"rows": html, "more": html}`.
pub fn trades_page(st: &State, store: &Store, p: u32, before: (u32, u32), maker: Option<[u8; 20]>) -> String {
    let pool = &st.pools[p as usize];
    let (page, more) = match maker {
        None => {
            let page = store.trades(p, before, TRADES);
            let more = more_button(&page);
            (page, more)
        }
        Some(m) => {
            let (page, last) = store.trades_by(p, m, before, TRADES, MAKER_SCAN);
            // More may follow while we stopped early, or the scan limit cut the search short.
            let more = match last {
                Some((b, i)) if page.len() == TRADES || store.trades(p, (b, i), 1).len() == 1 => format!(
                    r#"<button type="button" class="more" id="more" data-before="{b}-{i}">Load more trades</button>"#
                ),
                _ => String::new(),
            };
            (page, more)
        }
    };
    let rows: String = page.iter().map(|(k, t)| trade_row(st, pool, *k, t)).collect();
    serde_json::json!({"rows": rows, "more": more}).to_string()
}

/// A trades table row; `data-k` is its cursor (block-index) for paging.
pub fn trade_row(st: &State, pool: &Pool, (block, index): (u32, u32), t: &Trade) -> String {
    let (kind, cls) = match t.kind {
        TradeKind::Swap if t.buy => ("Buy", "u"),
        TradeKind::Swap => ("Sell", "d"),
        TradeKind::Add => ("Add", "lq"),
        TradeKind::Remove => ("Remove", "lq"),
    };
    let amount = s(|o| {
        fmt::compact(o, t.base);
        let _ = write!(o, " {}", Esc(sym(st, pool.base())));
        // Liquidity moves both tokens.
        if t.kind != TradeKind::Swap {
            o.push_str(" + ");
            fmt::compact(o, t.quote);
            let _ = write!(o, " {}", Esc(sym(st, pool.quote())));
        }
    });
    let price = if t.kind == TradeKind::Swap { s(|o| fmt::usd(o, t.price_usd as f64)) } else { "–".into() };
    format!(
        r#"<tr data-k="{block}-{index}" data-t="{ts}" data-u="{pu}" data-n="{pn}" data-s="{side}"><td class="l m">{time}</td><td class="l {cls}">{kind}</td><td class="{cls}">{price}</td><td>{amount}</td><td>{total}</td><td class="hide-s"><button type="button" class="mk mono" data-maker="{maker}" title="Only this wallet's trades">{maker_s}</button></td><td class="hide-s"><a class="ic sm" href="{EXPLORER}/tx/{tx}" rel="nofollow" target="_blank" aria-label="Transaction {tx_s} on Basescan" title="Transaction">{ext}</a></td></tr>"#,
        time = s(|o| fmt::date_clock(o, t.ts as i64)),
        ts = t.ts,
        pu = t.price_usd,
        pn = t.price_native,
        side = match t.kind {
            TradeKind::Swap if t.buy => "b",
            TradeKind::Swap => "s",
            _ => "l",
        },
        total = s(|o| fmt::usd_compact(o, t.usd as f64)),
        maker = hex(&t.maker),
        maker_s = short(&t.maker),
        tx = hex(&t.tx),
        tx_s = &hex(&t.tx)[..10],
        ext = icon("external-link"),
    )
}

const SEARCH_FORM: &str = r##"<form action="/search" role="search" class="sf"><svg class="i" aria-hidden="true"><use href="#i-search"/></svg><input type="search" name="q" placeholder="Search token, pair or address" aria-label="Search tokens, pairs and addresses" autocomplete="off" enterkeyhint="search"></form>"##;

/// The token's icon (see `icons`), or a round badge with its first letter, colored by
/// its address.
fn avatar(st: &State, token: u32, large: bool) -> String {
    let t = &st.tokens[token as usize];
    if t.icon {
        let (cls, px, lazy) = if large { (" lg", 40, "") } else { ("", 26, r#" loading="lazy""#) };
        return format!(
            r#"<img class="av{cls}" src="/i/{}.png" width="{px}" height="{px}" alt=""{lazy} decoding="async">"#,
            hex(&t.addr)
        );
    }
    let hue = (u16::from_be_bytes([t.addr[18], t.addr[19]]) % 360) as u32;
    let letter: String = t
        .symbol
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into());
    format!(
        r#"<span class="av{}" style="--h:{hue}" aria-hidden="true">{}</span>"#,
        if large { " lg" } else { "" },
        Esc(&letter)
    )
}

pub(crate) fn pair_name(st: &State, pool: &Pool, dex: bool) -> String {
    let mut out = format!(
        r#"<span class="pairname">{}<span><b>{}</b><span class="q">/{}</span></span>"#,
        avatar(st, pool.base(), false),
        Esc(sym(st, pool.base())),
        Esc(sym(st, pool.quote()))
    );
    if dex {
        let _ = write!(out, "<small>{}</small>", pool.dex());
    }
    out.push_str("</span>");
    out
}

pub fn home(app: &App, st: &State, f: &Filter, pools: &[u32]) -> String {
    let empty = if f.tab() == Tab::Watch {
        "Star pairs (★) to keep them here. The list lives in this browser."
    } else {
        "Nothing matches. Try other filters, or wait: the indexer may still be catching up."
    };
    // The watchlist's src carries the sort too; app.js appends the starred pairs.
    let src = format!("/api/list?{}", f.query(&[]));
    let body = format!(
        r#"<section class="hero"><div class="wrap"><h1>Every token on Base, live.</h1><p>Prices, charts and trades for every Uniswap, Aerodrome and PancakeSwap pool, straight from the chain. Free and open source.</p>{SEARCH_FORM}<div class="links"><a href="/api">Free API</a> · <a href="{REPO}">Source code</a></div></div></section><main class="wrap">{}{}</main>"#,
        lists::controls(f),
        lists::table(st, pools, Some(f), &src, empty),
    );
    let q = f.query(&[]);
    let path = if q.is_empty() { "/".to_string() } else { format!("/?{q}") };
    let head = Head {
        title: format!("{SITE}: live prices and charts for every token on Base"),
        desc: "Fast, free and open-source DEX screener for Base. Live charts, trades, liquidity and volume for every Uniswap, Aerodrome and PancakeSwap pool.".into(),
        path,
        image: Some(format!("{}/og.png", app.cfg.public_url)),
        miniapp: None,
        player: None,
        embed: false,
    };
    layout(app, st.head_ts, &head, &body)
}

pub fn search(app: &App, st: &State, q: &str, pools: &[u32]) -> String {
    let body = format!(
        r#"<main class="wrap"><h1>Results for “{}”</h1><p class="sub">{} pairs, most liquid first.</p>{}</main>"#,
        Esc(q),
        pools.len(),
        lists::table(st, pools, None, &format!("/api/list?q={}", urlencode(q)), "No matches.")
    );
    let head = Head {
        title: format!("{} on Base | {SITE}", q),
        desc: format!("Base pairs matching {q}: live price, chart, liquidity and volume."),
        path: format!("/search?q={}", urlencode(q)),
        image: None,
        miniapp: None,
        player: None,
        embed: false,
    };
    layout(app, st.head_ts, &head, &body)
}

/// /api: how to use the public JSON API (see `api`).
pub fn api_docs(app: &App, st: &State) -> String {
    let base = &app.cfg.public_url;
    // Live examples with a real pair and token.
    let weth = st.token_ix.get(&crate::abi::WETH).map(|&t| &st.tokens[t as usize]);
    let pair =
        weth.filter(|t| t.best_pool != NONE).map(|t| st.pools[t.best_pool as usize].key()).unwrap_or("0x…".into());
    let token = hex(&crate::abi::WETH);
    let endpoint = |path: &str, what: &str, params: &[(&str, &str)], example: &str| {
        let params: String = params
            .iter()
            .map(|(n, d)| format!("<tr><td class=\"l mono\">{n}</td><td class=\"l\">{d}</td></tr>"))
            .collect();
        let params = if params.is_empty() {
            params
        } else {
            format!(r#"<table class="params"><tbody>{params}</tbody></table>"#)
        };
        format!(
            r#"<section class="card ep"><h2><span class="get">GET</span> <code>{path}</code></h2><p>{what}</p>{params}<p class="ex">Try it: <a class="mono" href="{example}" rel="nofollow">{example}</a></p></section>"#
        )
    };
    let body = format!(
        r#"<main class="wrap docs"><h1>Free API</h1><p class="sub">Everything on {SITE} as JSON: pairs, tokens, candles, trades and lists for every pool on Base. No key, no sign-up, and any website may call it (CORS is open).</p>
<div class="card note-card"><p><b>Limits.</b> {limit} requests a minute per IP. Responses are cached at the edge for a few seconds (older candles and trades for a day), and cached answers don't count. Past the limit you get <code>429</code> with <code>Retry-After</code>; <code>X-RateLimit-Remaining</code> says how many are left. Need more? The code is open source: run your own.</p><p><b>Conventions.</b> Pairs are pool addresses (v4 pools: their 32-byte id). Times are Unix seconds. Prices in USD use on-chain stablecoin pools; a pair whose quote token has no USD price yet has <code>price_usd: null</code>. <code>trusted</code> is true for pools from known DEX factories (and v4); only those set token prices.</p></div>
{}{}{}{}{}{}
<p class="sub">Questions or ideas: <a href="{REPO}">open an issue</a>.</p></main>"#,
        endpoint(
            "/api/v1/search?q=",
            "Pairs for a symbol, name or address, best match first (up to 20).",
            &[("q", "symbol (degen), name, or a pair or token address")],
            &format!("{base}/api/v1/search?q=degen")
        ),
        endpoint(
            "/api/v1/pairs/{pair}",
            "One pair: tokens, price, liquidity, FDV, pooled amounts, and change, volume and buys/sells over 5m, 1h, 6h and 24h. A token address gives its deepest pair.",
            &[],
            &format!("{base}/api/v1/pairs/{pair}")
        ),
        endpoint(
            "/api/v1/pairs/{pair}/candles",
            "OHLCV candles, oldest first: <code>[time, open, high, low, close, volume_usd]</code>.",
            &[
                ("res", "1m, 5m, 15m (default), 1h, 4h or 1d"),
                ("unit", "usd (default) or native (in the quote token)"),
                ("before", "only candles before this time (paging back)"),
                ("limit", "1–1000, default 300")
            ],
            &format!("{base}/api/v1/pairs/{pair}/candles?res=1h")
        ),
        endpoint(
            "/api/v1/pairs/{pair}/trades",
            "Trades, newest first, including liquidity adds and removals (<code>type</code>: buy, sell, add, remove). Pass <code>next</code> back as <code>before</code> for older ones.",
            &[
                ("before", "a trade id (block-index) to page from"),
                ("maker", "only this wallet's trades"),
                ("limit", "1–500, default 100")
            ],
            &format!("{base}/api/v1/pairs/{pair}/trades?limit=20")
        ),
        endpoint(
            "/api/v1/tokens/{token}",
            "A token: price, supply, FDV, description and links, safety checks, and its pairs on known DEXes (deepest first, up to 50).",
            &[],
            &format!("{base}/api/v1/tokens/{token}")
        ),
        endpoint(
            "/api/v1/lists/{tab}",
            "The home lists: trending, top, gainers, losers or new (up to 100 pairs), with the same filters as the site.",
            &[
                ("w", "window for trending, gainers and losers: 5m, 1h, 6h, 24h"),
                ("liq, vol, txns", "minimum liquidity, 24h volume (USD) and 24h transactions"),
                ("age", "created at most this many hours ago"),
                ("sort, dir", "price, c5m, c1h, c6h, c24h, vol, liq, fdv, txns or age; dir=asc for ascending")
            ],
            &format!("{base}/api/v1/lists/gainers?w=1h&liq=50000")
        ),
        limit = crate::api::LIMIT,
    );
    let head = Head {
        title: format!("Free API for Base DEX data | {SITE}"),
        desc: "Free JSON API for every Base DEX pool: pairs, tokens, candles, trades and lists. No key, CORS open."
            .into(),
        path: "/api".into(),
        image: Some(format!("{}/og.png", app.cfg.public_url)),
        miniapp: None,
        player: None,
        embed: false,
    };
    layout(app, st.head_ts, &head, &body)
}

pub fn not_found(app: &App, head_ts: u32) -> String {
    let head = Head {
        title: format!("Not found | {SITE}"),
        desc: "This pair or token isn't indexed.".into(),
        path: "/404".into(),
        image: None,
        miniapp: None,
        player: None,
        embed: false,
    };
    layout(
        app,
        head_ts,
        &head,
        r#"<main class="wrap"><h1>Not found</h1><p class="sub">We don't index a Base pair or token at that address. Try the search box.</p></main>"#,
    )
}

/// Candles of a pool for the chart: in USD when the quote token has a price (unless
/// `native`), else in the quote token.
pub fn chart_data(
    st: &State,
    store: &Store,
    p: u32,
    ri: usize,
    before: u32,
    limit: usize,
    native: bool,
) -> Vec<chart::Candle> {
    let raw = store.candles(p, ri, 0, before, limit);
    let usd = !native && raw.last().is_some_and(|(_, c)| c.c.is_finite()) && st.pools[p as usize].price_usd.is_finite();
    raw.iter()
        .map(|(t, c)| {
            let (o, h, l, cl) = if usd { (c.o, c.h, c.l, c.c) } else { (c.no, c.nh, c.nl, c.nc) };
            chart::Candle { t: *t as f64, o: o as f64, h: h as f64, l: l as f64, c: cl as f64, v: c.v as f64 }
        })
        .filter(|c| c.o.is_finite() && c.c.is_finite())
        .collect()
}

/// Uniswap's swap page on Base, buying the base token with the quote token (ETH for WETH).
pub fn buy_url(st: &State, pool: &Pool) -> String {
    let token = |t: u32| {
        let a = st.tokens[t as usize].addr;
        if a == crate::abi::WETH { "ETH".to_string() } else { hex(&a) }
    };
    format!(
        "https://app.uniswap.org/swap?chain=base&inputCurrency={}&outputCurrency={}",
        token(pool.quote()),
        token(pool.base())
    )
}

/// A token's description and links (see `icons`) as HTML, each empty when unknown.
fn profile(store: &Store, token: crate::abi::Addr) -> (String, String) {
    let profile: serde_json::Value =
        store.profiles.get(token).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default();
    let desc = match profile["d"].as_str() {
        Some(d) if !d.is_empty() => format!(r#"<p class="desc">{}</p>"#, Esc(d)),
        _ => String::new(),
    };
    let links: String = profile["l"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let url = l[1].as_str()?;
            let kind = crate::icons::link_kind(url)?;
            let i = match kind {
                "X" => "xlogo",
                "Telegram" => "telegram",
                "Discord" => "discord",
                "Farcaster" => "farcaster",
                "GitHub" => "github",
                _ => "globe",
            };
            Some(format!(
                r#"<a class="btn ic" href="{}" target="_blank" rel="nofollow ugc noopener noreferrer" aria-label="{kind}" title="{kind}">{}</a>"#,
                Esc(url),
                icon(i)
            ))
        })
        .collect();
    let links = if links.is_empty() { links } else { format!(r#"<div class="links">{links}</div>"#) };
    (desc, links)
}

/// A token's page: price, totals across its pools, what it says about itself, safety
/// checks and every pair it trades in.
pub fn token(app: &App, st: &State, store: &Store, t: u32) -> String {
    let tok = &st.tokens[t as usize];
    let best = best_pool(st, t).unwrap_or(NONE);
    let mut pools: Vec<u32> = st.base_pools.get(&t).cloned().unwrap_or_default();
    pools.sort_unstable_by(|a, b| st.liq(*b).total_cmp(&st.liq(*a)));
    let (liq, vol) = pools.iter().fold((0.0, 0.0), |(l, v), &p| {
        let m = metrics(st, p);
        (l + if m.liq.is_finite() { m.liq } else { 0.0 }, v + if m.w[3].vol.is_finite() { m.w[3].vol } else { 0.0 })
    });
    let symbol = sym(st, t);
    let name = if tok.name.is_empty() { symbol } else { &tok.name };
    let (desc, links) = profile(store, tok.addr);
    let (ch, buy, safety) = if best == NONE {
        (f64::NAN, String::new(), String::new())
    } else {
        let pool = &st.pools[best as usize];
        let buy = format!(
            r#"<a class="btn btn-primary buy" href="{}" target="_blank" rel="noopener nofollow" title="Swap on Uniswap">{}Buy</a>"#,
            buy_url(st, pool),
            icon("arrow-left-right")
        );
        (change(&metrics(st, best), 3), buy, safety_card(st, store, best))
    };
    pools.truncate(50);
    let keys: Vec<String> = pools.iter().map(|&p| st.pools[p as usize].key()).collect();
    let table =
        lists::table(st, &pools, None, &format!("/api/list?pairs={}", keys.join(",")), "No pairs on known DEXes yet.");
    let stat = |label: &str, v: String| format!("<div><dt>{label}</dt><dd>{v}</dd></div>");
    let body = format!(
        r#"<main class="wrap"><div class="head"><div class="id">{av}<div class="meta"><h1>{name_e}</h1><span class="name">{sym_e} · <a class="mono m" href="{EXPLORER}/token/{addr}" rel="nofollow">{short}</a></span></div></div><div class="px"><div class="pv"><div class="big">{price}</div></div><span class="chg"><span class="{cls}">{ch}</span></span>{buy}</div></div><div class="tgrid"><dl class="card kv kv4">{stats}</dl><div class="card tok"><div class="ch2"><h2>About {name_e}</h2>{links}</div>{about}<div class="info"><p><span>Contract</span><span class="addr"><span class="mono">{short}</span><button type="button" class="ic sm" data-copy="{addr}" aria-label="Copy contract address" title="Copy address">{copy}</button><a class="ic sm" href="{EXPLORER}/token/{addr}" rel="nofollow" target="_blank" aria-label="Token on Basescan" title="Basescan">{ext}</a></span></p><p><span>Decimals</span><span>{decimals}</span></p><p><span>Pairs on known DEXes</span><span>{n_pairs}</span></p><p><span>First traded</span><span>block {first}</span></p></div></div>{safety}</div><h2 class="sect">Pairs <span class="m">{n_pairs}</span></h2>{table}</main>"#,
        av = avatar(st, t, true),
        // Say so rather than leave an empty card.
        about = if desc.is_empty() {
            r#"<p class="desc m">No description or links published yet.</p>"#.to_string()
        } else {
            desc.clone()
        },
        n_pairs = pools.len(),
        decimals = tok.decimals,
        first = s(|o| fmt::uint(o, tok.first_block as u64)),
        copy = icon("copy"),
        ext = icon("external-link"),
        name_e = Esc(name),
        sym_e = Esc(symbol),
        addr = hex(&tok.addr),
        short = short(&tok.addr),
        price = s(|o| fmt::usd(o, tok.usd)),
        cls = dir(ch),
        ch = s(|o| fmt::pct(o, ch)),
        stats = [
            stat("Liquidity", s(|o| fmt::usd_compact(o, liq))),
            stat("Volume 24h", s(|o| fmt::usd_compact(o, vol))),
            stat("FDV", s(|o| fmt::usd_compact(o, tok.supply * tok.usd))),
            stat("Supply", s(|o| fmt::compact(o, tok.supply))),
        ]
        .concat(),
    );
    let head = Head {
        title: format!("{symbol} price {} | {name} on Base | {SITE}", s(|o| fmt::usd(o, tok.usd))),
        desc: format!(
            "{name} ({symbol}) on Base: price, liquidity and volume across {} pairs, links and safety checks.",
            pools.len()
        ),
        path: format!("/base/token/{}", hex(&tok.addr)),
        image: (best != NONE).then(|| {
            format!("{}/og/base/{}.png?v={}", app.cfg.public_url, st.pools[best as usize].key(), st.head_ts / 300)
        }),
        miniapp: None,
        player: None,
        embed: false,
    };
    layout(app, st.head_ts, &head, &body)
}

/// The base token: what it says about itself (see `icons`), its supply, and what this
/// pool holds.
fn token_card(st: &State, store: &Store, pool: &Pool, fs: &[(String, String, &'static str)]) -> String {
    let base = &st.tokens[pool.base() as usize];
    let (desc, links) = profile(store, base.addr);
    format!(
        r#"<div class="card tok"><div class="ch2"><a class="hl2" href="/base/token/{addr}" title="Token page"><h2>About {name}</h2>{chev}</a>{links}</div>{desc}<dl class="mini"><div><dt>Supply</dt><dd>{supply}</dd></div><div><dt>Pooled {bs}</dt><dd>{bb}</dd></div><div><dt>Pooled {qs}</dt><dd>{qb}</dd></div></dl></div>"#,
        chev = icon("chevron-right"),
        name = Esc(if base.name.is_empty() { sym(st, pool.base()) } else { &base.name }),
        addr = hex(&base.addr),
        supply = field(fs, "sup"),
        bs = Esc(sym(st, pool.base())),
        qs = Esc(sym(st, pool.quote())),
        bb = field(fs, "pb"),
        qb = field(fs, "pq"),
    )
}

/// What the chain says about the base token's risks (see `safety`), and whether anyone
/// has managed to sell it lately.
fn safety_card(st: &State, store: &Store, p: u32) -> String {
    let pool = &st.pools[p as usize];
    let base = &st.tokens[pool.base() as usize];
    let v: serde_json::Value =
        store.safety.get(base.addr).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default();
    // (label, value, Some(true) fine / Some(false) a warning / None neutral)
    let mut rows: Vec<(&str, String, Option<bool>)> = Vec::new();
    if v["t"].is_null() {
        rows.push(("Contract checks", "Not run yet".into(), None));
    } else {
        rows.push(match v["owner"].as_str().unwrap_or("none") {
            "renounced" => ("Owner", "Renounced".into(), Some(true)),
            "none" => ("Owner", "None".into(), Some(true)),
            a => (
                "Owner",
                format!(
                    r#"<a class="mono" href="{EXPLORER}/address/{0}" rel="nofollow">{1}</a>"#,
                    Esc(a),
                    Esc(&a
                        .chars()
                        .take(6)
                        .chain("…".chars())
                        .chain(a.chars().skip(a.len().saturating_sub(4)))
                        .collect::<String>())
                ),
                Some(false),
            ),
        });
        let flag = |key: &str, yes: &str, no: &str| {
            let on = v[key].as_bool().unwrap_or(false);
            ((if on { yes } else { no }).to_string(), Some(!on))
        };
        let (t, ok) = flag("upgradeable", "Yes", "No");
        rows.push(("Upgradeable", t, ok));
        let (t, ok) = flag("mint", "Yes", "No");
        rows.push(("Can mint", t, ok));
        let on = v["pause"].as_bool().unwrap_or(false) || v["blacklist"].as_bool().unwrap_or(false);
        rows.push(("Can pause or blacklist", (if on { "Yes" } else { "No" }).into(), Some(!on)));
        match &v["lp"] {
            lp if lp["pool"].as_str() == Some(pool.key().as_str()) => {
                let burned = lp["burned"].as_f64().unwrap_or(0.0);
                rows.push((
                    "LP burned",
                    s(|o| fmt::pct(o, burned)).trim_start_matches('+').to_string(),
                    Some(burned >= 0.95),
                ));
            }
            // Concentrated liquidity has positions, not LP tokens to burn.
            _ => {}
        }
    }
    let (_, buyers, sellers) = makers(store, p, st.head_ts)[3];
    rows.push(("Sellers, 24h", s(|o| fmt::uint(o, sellers as u64)), if buyers > 0 { Some(sellers > 0) } else { None }));
    let warnings = rows.iter().filter(|r| r.2 == Some(false)).count();
    let rows: String = rows
        .iter()
        .map(|(label, value, ok)| {
            let (cls, i, what) = match ok {
                Some(true) => ("ok", "circle-check", "Fine"),
                Some(false) => ("warn", "triangle-alert", "Warning"),
                None => ("", "circle-minus", "Unknown"),
            };
            format!(
                r##"<li class="{cls}"><svg class="i" role="img" aria-label="{what}"><use href="#i-{i}"/></svg><span>{label}</span><b>{value}</b></li>"##
            )
        })
        .collect();
    let summary = match warnings {
        0 => r#"<span class="pill ok">No flags</span>"#.to_string(),
        n => format!(r#"<span class="pill warn">{n} flag{}</span>"#, if n == 1 { "" } else { "s" }),
    };
    format!(
        r#"<div class="card safety"><div class="ch2"><h2>{shield}Safety</h2>{summary}</div><ul class="checks">{rows}</ul><p class="fine">Read from the contract and recent trades: a heuristic, not an audit.</p></div>"#,
        shield = icon("shield-check"),
    )
}

/// The base token's other pools on known DEXes, deepest first.
fn other_pairs(st: &State, p: u32) -> String {
    let pool = &st.pools[p as usize];
    let mut others: Vec<(f64, u32)> = st
        .base_pools
        .get(&pool.base())
        .into_iter()
        .flatten()
        .filter(|&&q| q != p)
        .map(|&q| (st.liq(q), q))
        .filter(|(liq, _)| *liq >= 1.0)
        .collect();
    if others.is_empty() {
        return String::new();
    }
    others.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    let rows: String = others
        .iter()
        .take(6)
        .map(|&(liq, q)| {
            let o = &st.pools[q as usize];
            format!(
                r#"<a href="/base/{key}"><span><b>{bs}</b><span class="m">/{qs}</span> <small>{dex}</small></span><span>{liq}</span></a>"#,
                key = o.key(),
                bs = Esc(sym(st, o.base())),
                qs = Esc(sym(st, o.quote())),
                dex = o.dex(),
                liq = s(|x| fmt::usd_compact(x, liq)),
            )
        })
        .collect();
    format!(
        r#"<div class="card pairs"><div class="ch2"><h2>Other pairs</h2><a class="more-link" href="/base/token/{}">All</a></div>{rows}</div>"#,
        hex(&st.tokens[pool.base() as usize].addr)
    )
}

/// The chart toolbar's options (applied by app.js): price or market cap, USD or the quote
/// token, candles or line, an indicators menu, and fullscreen. Choices that would change
/// nothing are left out: no supply, no USD price, or a stablecoin quote (USD is the same).
fn chart_tools(st: &State, pool: &Pool) -> String {
    let seg = |opt: &str, a: &str, b: &str, label: &str| {
        format!(
            r#"<span class="seg" role="group" aria-label="{label}"><button type="button" data-o="{opt}" data-v="0" aria-pressed="true">{a}</button><button type="button" data-o="{opt}" data-v="1" aria-pressed="false">{b}</button></span>"#
        )
    };
    let mut out = String::from(r#"<div class="tools" role="toolbar" aria-label="Chart options">"#);
    if st.tokens[pool.base() as usize].supply > 0.0 {
        out.push_str(&seg("mcap", "Price", "MCap", "Price or market cap"));
    }
    let quote = &st.tokens[pool.quote() as usize];
    if pool.price_usd.is_finite() && !crate::state::STABLES.contains(&quote.addr) {
        out.push_str(&seg("native", "USD", &Esc(sym(st, pool.quote())).to_string(), "Price in"));
    }
    let _ = write!(
        out,
        r#"<span class="seg" role="group" aria-label="Chart type"><button type="button" class="ic" data-o="line" data-v="0" aria-pressed="true" aria-label="Candles" title="Candles">{}</button><button type="button" class="ic" data-o="line" data-v="1" aria-pressed="false" aria-label="Line" title="Line">{}</button></span>"#,
        icon("chart-candlestick"),
        icon("chart-line"),
    );
    let _ = write!(
        out,
        r#"<button type="button" class="ic ind" popovertarget="ind" aria-label="Indicators" title="Indicators">{}<b class="cnt" hidden></b></button><div id="ind" class="menu" popover><p>Indicators</p><label><input type="checkbox" data-o="ma"><i class="sw ma"></i>Moving averages <small>20, 50</small></label><label><input type="checkbox" data-o="vwap"><i class="sw vw"></i>VWAP <small>from each day's start</small></label><label><input type="checkbox" data-o="rsi"><i class="sw rs"></i>RSI <small>14</small></label><hr><label><input type="checkbox" data-o="log">Log scale</label></div><button type="button" class="ic full" data-o="full" aria-label="Fullscreen" title="Fullscreen">{}</button></div>"#,
        icon("activity"),
        icon("maximize"),
    );
    out
}

/// Candles as compact JSON: [[t,o,h,l,c,v],…]
pub fn candles_json(cs: &[chart::Candle]) -> String {
    let mut out = String::from("[");
    for (i, c) in cs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{},{},{},{},{},{}]", c.t, c.o as f32, c.h as f32, c.l as f32, c.c as f32, c.v as f32);
    }
    out.push(']');
    out
}

#[allow(clippy::too_many_arguments)]
pub fn pair(app: &App, st: &State, store: &Store, p: u32, ri: usize, embed: bool) -> String {
    let pool = &st.pools[p as usize];
    let (base, quote) = (&st.tokens[pool.base() as usize], &st.tokens[pool.quote() as usize]);
    let (bs, qs) = (sym(st, pool.base()), sym(st, pool.quote()));
    let key = pool.key();
    let fs = fields_full(st, store, p);
    let f = |id: &str| field(&fs, id);
    let candles = chart_data(st, store, p, ri, u32::MAX, CHART_CANDLES, false);
    let svg = s(|o| chart::render(&candles, &chart::View::latest(candles.len(), 960.0, 440.0, RES[ri] as f64), o));
    let res_nav: String = RES_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let cur = if i == ri { r#" aria-current="true""# } else { "" };
            format!(r#"<a href="?res={n}" data-res="{}" rel="nofollow"{cur}>{n}</a>"#, RES[i])
        })
        .collect();
    let chart = format!(
        r#"<div class="card chartcard"><div class="cbar"><nav class="res" aria-label="Candle size">{res_nav}</nav>{tools}</div><div class="plot"><div id="chart" data-pool="{key}" data-res="{res}" data-wasm="{wasm}" data-supply="{supply}">{svg}</div><button type="button" class="ic latest" hidden aria-label="Back to the latest candles" title="Latest">{latest}</button></div></div><script id="cd" type="application/json">{json}</script>"#,
        tools = chart_tools(st, pool),
        latest = icon("chevrons-right"),
        supply = base.supply,
        res = RES[ri],
        wasm = app.wasm_url,
        json = candles_json(&candles),
    );
    let price_head = format!(r#"<span class="big">{}</span> <span class="chg">{}</span>"#, f("p"), f("c24h"));
    let url = format!("{}/base/{key}", app.cfg.public_url);
    let m = metrics(st, p);
    let price_text = price_label(st, p, &m);
    let title = pair_title(st, p);
    let desc = format!(
        "{bs} ({}) price {}, 24h volume {}, liquidity {} on {}. Live chart and trades for the {bs}/{qs} pool on Base.",
        if base.name.is_empty() { bs } else { &base.name },
        price_text,
        s(|o| fmt::usd_compact(o, m.w[3].vol)),
        s(|o| fmt::usd_compact(o, m.liq)),
        pool.dex(),
    );
    let v = st.head_ts / 300;
    let head = Head {
        title,
        desc,
        // Embeds point search engines at the full pair page.
        path: format!("/base/{key}"),
        image: Some(format!("{}/og/base/{key}.png?v={v}", app.cfg.public_url)),
        miniapp: Some((format!("{}/og/base/{key}.png?s=fc&v={v}", app.cfg.public_url), url.clone())),
        player: Some(format!("{}/embed/base/{key}", app.cfg.public_url)),
        embed,
    };

    if embed {
        let body = format!(
            r#"<div class="eh">{name}{price_head}<a class="brand" href="{url}" target="_blank" rel="noopener">{SITE}</a></div>{chart}"#,
            name = pair_name(st, pool, false),
        );
        return layout(app, st.head_ts, &head, &body);
    }

    let trades = store.trades(p, (u32::MAX, u32::MAX), TRADES);
    let rows: String = trades.iter().map(|(k, t)| trade_row(st, pool, *k, t)).collect();
    let last = trades.first().map(|(k, _)| format!("{}-{}", k.0, k.1)).unwrap_or_else(|| "0-0".into());
    // Stats per window: tabs with the change, panels with volume and buy/sell bars.
    let wtabs: String = WINDOWS
        .iter()
        .map(|(n, _)| {
            let cur = *n == "24h";
            format!(
                r#"<button type="button" role="tab" aria-selected="{cur}" data-w="{n}"><span>{n}</span>{}</button>"#,
                f(&format!("c{n}"))
            )
        })
        .collect();
    let wpanels: String = WINDOWS
        .iter()
        .map(|(n, _)| {
            let bar = |label: &str, total: &str, a: &str, al: &str, b: &str, bl: &str, ratio: &str| {
                format!(
                    r#"<div class="split"><div class="sh"><span>{label}</span><b>{total}</b></div><div class="track">{ratio}</div><div class="sl"><span class="u">{a} {al}</span><span class="d">{b} {bl}</span></div></div>"#
                )
            };
            format!(
                r#"<div class="wp" data-w="{n}"{hidden}><div class="sh vol"><span>Volume</span><b>{vol}</b></div>{txns}{makers}</div>"#,
                hidden = if *n == "24h" { "" } else { " hidden" },
                vol = f(&format!("v{n}")),
                txns = bar("Txns", "", &f(&format!("b{n}")), "buys", &f(&format!("s{n}")), "sells", &f(&format!("rb{n}"))),
                makers = bar(
                    "Makers",
                    &f(&format!("k{n}")),
                    &f(&format!("kb{n}")),
                    "buyers",
                    &f(&format!("ks{n}")),
                    "sellers",
                    &f(&format!("rk{n}"))
                ),
            )
        })
        .collect();
    let addr_row = |label: &str, a: &str, link: &str| {
        format!(
            r#"<p><span>{label}</span><span class="addr"><span class="mono">{short}</span><button type="button" class="ic sm" data-copy="{a}" aria-label="Copy {label} address" title="Copy address">{copy}</button><a class="ic sm" href="{EXPLORER}/{link}" rel="nofollow" target="_blank" aria-label="{label} on Basescan" title="Basescan">{ext}</a></span></p>"#,
            short = if a.len() > 12 { format!("{}…{}", &a[..6], &a[a.len() - 4..]) } else { a.to_string() },
            copy = icon("copy"),
            ext = icon("external-link"),
        )
    };
    let pool_link = if pool.is_address() {
        format!("address/{key}")
    } else {
        format!("address/{}", hex(&crate::abi::V4_POOL_MANAGER))
    };
    let embed_code = format!(
        r#"<iframe src="{}/embed/base/{key}" width="100%" height="480" style="border:0;border-radius:12px" title="{} chart on {SITE}" loading="lazy" allowfullscreen></iframe>"#,
        app.cfg.public_url,
        Esc(&format!("{bs}/{qs}")),
    );
    let share = format!(
        "https://x.com/intent/post?text={}&url={}",
        urlencode(&format!("{bs} on Base")),
        urlencode(&format!("{url}?s={}", st.head_ts / 3600))
    );
    let body = format!(
        r#"<main class="wrap"><div class="head"><div class="id">{av}<div class="meta"><div class="t"><h1>{bs_e}<span class="q">/{qs_e}</span></h1><button type="button" class="ic star" data-star="{key}" aria-pressed="false" aria-label="Add to watchlist" title="Watchlist">{star}</button></div><span class="name"><a href="/base/token/{base_addr}">{bname}</a><span class="badge">{dex}</span></span></div></div><div class="px"><div class="pv"><div class="big">{p}</div><div class="pn">{pn}</div></div><span class="chg">{c24}</span><a class="btn btn-primary buy" href="{buy}" target="_blank" rel="noopener nofollow" title="Swap on Uniswap">{swap}Buy</a></div></div>
<div class="pair"><section class="top-l">{chart}</section>
<aside><div class="card stats"><dl class="kv"><div><dt>Price USD</dt><dd>{p}</dd></div><div><dt>Price {qs_e}</dt><dd>{pn}</dd></div><div><dt>Liquidity</dt><dd>{liq}</dd></div><div><dt>FDV</dt><dd>{fdv}</dd></div></dl><div class="wt" role="tablist" aria-label="Window">{wtabs}</div>{wpanels}</div>{safety}{token}{others}<div class="card info"><h2>Pair</h2>{pair_c}{base_c}{quote_c}<p><span>DEX</span><span>{dex}</span></p><p><span>Created</span><span>{created} ago</span></p></div><div class="card share"><h2>Share</h2><div class="row"><button type="button" class="btn" data-copy="{url}" title="Copy link">{link}Link</button><button type="button" class="btn" popovertarget="embed" title="Embed code">{code}Embed</button><a class="btn" href="{share}" target="_blank" rel="noopener" title="Post on X">{xlogo}Post</a></div><div id="embed" class="menu wide" popover><p>Embed this chart</p><textarea readonly aria-label="Embed code">{embed_e}</textarea><button type="button" class="btn btn-primary" data-copy="{embed_e}">{copy}Copy code</button></div></div></aside>
<section class="bot-l"><div class="card trades-card"><div class="tt" role="tablist" aria-label="Trades"><button type="button" role="tab" aria-selected="true" data-tab="trades">Trades</button><button type="button" role="tab" aria-selected="false" data-tab="traders">Top traders</button></div><div id="traders" hidden></div><div class="scroll"><table class="trades"><thead><tr><th class="l">Time (UTC)</th><th class="l">Type</th><th>Price</th><th>Amount</th><th>Value</th><th class="hide-s">Maker</th><th class="hide-s"><span class="sr">Transaction</span></th></tr></thead><tbody id="tr" data-last="{last}">{rows}</tbody></table></div>{more}</div></section></div></main>"#,
        av = avatar(st, pool.base(), true),
        star = icon("star"),
        swap = icon("arrow-left-right"),
        link = icon("link"),
        code = icon("code-xml"),
        xlogo = icon("xlogo"),
        copy = icon("copy"),
        base_addr = hex(&base.addr),
        buy = buy_url(st, pool),
        more = more_button(&trades),
        token = token_card(st, store, pool, &fs),
        safety = safety_card(st, store, p),
        others = other_pairs(st, p),
        bs_e = Esc(bs),
        c24 = f("c24h"),
        dex = pool.dex(),
        bname = Esc(if base.name.is_empty() { bs } else { &base.name }),
        pn = f("pn"),
        p = f("p"),
        liq = f("liq"),
        fdv = f("fdv"),
        qs_e = Esc(qs),
        pair_c = addr_row("Pair", &key, &pool_link),
        base_c = addr_row(&format!("{}", Esc(bs)), &hex(&base.addr), &format!("token/{}", hex(&base.addr))),
        quote_c = addr_row(&format!("{}", Esc(qs)), &hex(&quote.addr), &format!("token/{}", hex(&quote.addr))),
        created = s(|o| fmt::age(o, st.head_ts as i64 - pool.created_ts as i64)),
        embed_e = Esc(&embed_code),
    );
    layout(app, st.head_ts, &head, &body)
}

/// A live update for a pair page: changed fields, the latest candle of every
/// resolution, and new trade rows. Returns the JSON and the newest trade key.
pub fn live(st: &State, store: &Store, p: u32, after: (u32, u32)) -> (String, (u32, u32)) {
    let pool = &st.pools[p as usize];
    let fs: serde_json::Map<String, serde_json::Value> =
        fields_full(st, store, p).into_iter().map(|(id, text, cls)| (id, serde_json::json!([text, cls]))).collect();
    // The latest candle of every resolution, in USD ("c") and in the quote token ("n").
    let latest = |native: bool| -> serde_json::Map<String, serde_json::Value> {
        RES.iter()
            .enumerate()
            .filter_map(|(ri, res)| {
                let c = *chart_data(st, store, p, ri, u32::MAX, 1, native).last()?;
                Some((res.to_string(), serde_json::json!([c.t, c.o, c.h, c.l, c.c, c.v])))
            })
            .collect()
    };
    let (candles, native) = (latest(false), latest(true));
    // The newest 50 trades after `after` (a page shows 50), newest first.
    let trades: Vec<_> = if after == (0, 0) {
        Vec::new()
    } else {
        store.trades(p, (u32::MAX, u32::MAX), TRADES).into_iter().take_while(|(k, _)| *k > after).collect()
    };
    let newest = trades.first().map(|(k, _)| *k).unwrap_or(after);
    let rows: String = trades.iter().map(|(k, t)| trade_row(st, pool, *k, t)).collect();
    (serde_json::json!({"f": fs, "c": candles, "n": native, "t": rows, "T": pair_title(st, p)}).to_string(), newest)
}

pub fn sitemap(app: &App, st: &State) -> String {
    let mut pools: Vec<(u32, f64)> = st
        .stats
        .keys()
        .filter(|&&p| st.pools[p as usize].trusted())
        .map(|&p| (p, st.liq(p)))
        .filter(|(_, l)| *l >= 10_000.0)
        .collect();
    pools.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#,
    );
    let _ = write!(out, "<url><loc>{}/</loc></url>", app.cfg.public_url);
    for (p, _) in pools.iter().take(20_000) {
        let _ = write!(out, "<url><loc>{}/base/{}</loc></url>", app.cfg.public_url, st.pools[*p as usize].key());
    }
    out.push_str("</urlset>");
    out
}

pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// The pool a token links to, if it has any.
/// Where a token links to: its deepest pool on a known DEX where it is the base (so the
/// page shows its own price: USDC goes to a USDC/stable pair at $1, not WETH/USDC at ETH's
/// price), else the deepest pool it's in at all.
pub fn best_pool(st: &State, token: u32) -> Option<u32> {
    own_pool(st, token).or_else(|| {
        let p = st.tokens.get(token as usize)?.best_pool;
        (p != NONE).then_some(p)
    })
}

/// A token's best-ranked (`State::pair_rank`) pool on a known DEX where it is the base (at
/// least $1k deep).
pub fn own_pool(st: &State, token: u32) -> Option<u32> {
    let ps = st.base_pools.get(&token)?;
    ps.iter()
        .map(|&p| (st.pair_rank(p), p))
        .filter(|((_, l), _)| *l >= 1_000.0)
        .max_by(|(a, _), (b, _)| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
        .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_profit() {
        // Bought 10 at $1, sold 4 at $2, holds 6 at $3: 4 + 12 = $16 on $10 paid.
        let w = Wallet { bought: 10.0, got: 10.0, sold: 8.0, gave: 4.0, buys: 1, sells: 1 };
        assert!((w.pnl(3.0).unwrap() - 16.0).abs() < 1e-9);
        // Sold 268 having bought 7 here: only the 7 count.
        let w = Wallet { bought: 7.0, got: 7.0, sold: 268.0 * 1.5, gave: 268.0, buys: 1, sells: 9 };
        assert!((w.pnl(1.5).unwrap() - 7.0 * 0.5).abs() < 1e-9);
        // Only sells: no profit to speak of.
        assert!(Wallet { sold: 100.0, gave: 50.0, sells: 3, ..Default::default() }.pnl(2.0).is_none());
    }

    #[test]
    fn escapes_and_css() {
        assert_eq!(format!("{}", Esc(r#"<b>"x"&'</b>"#)), "&lt;b&gt;&quot;x&quot;&amp;&#39;&lt;/b&gt;");
        let css = page_css();
        assert!(css.contains("--up:#0a7a3d") && css.contains("fill:var(--up)") && !css.contains("{up}"));
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
