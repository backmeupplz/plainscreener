//! Server-rendered HTML. Every number on a page is formatted here (or in `chart::fmt`),
//! and live updates reuse the same functions, so pages and updates always agree.

use crate::App;
use crate::abi::hex;
use crate::model::{Pool, RES, RES_NAMES, Trade, Window};
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
pub const PALETTE: [(&str, &str, &str); 23] = [
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
];

/// Chart styles, with `{name}` standing for a palette color.
const CHART_CSS: &str = ".ch .g{stroke:{grid};fill:none}.ch .u{fill:{up}}.ch .d{fill:{down}}\
.ch .vu{fill:{up};opacity:.28}.ch .vd{fill:{down};opacity:.28}.ch text{font-size:12px;font-family:system-ui,Inter,sans-serif}\
.ch .a{fill:{muted}}.ch .l{fill:none;stroke-dasharray:2 3}.ch .l.u{stroke:{up}}.ch .l.d{stroke:{down}}\
.ch .x{fill:none;stroke:{muted};stroke-dasharray:3 3}.ch .t{fill:{fg}}.ch .tc{fill:{bg}}.ch .tt{fill:{white}}";

/// Chart CSS with colors from `value(name)`.
pub fn chart_css(value: impl Fn(&str) -> String) -> String {
    PALETTE.iter().fold(CHART_CSS.to_string(), |css, (name, ..)| css.replace(&format!("{{{name}}}"), &value(name)))
}

/// The whole page stylesheet, inlined into every page.
pub fn page_css() -> String {
    let vars = |dark: bool| {
        PALETTE.iter().map(|(n, l, d)| format!("--{n}:{}", if dark { d } else { l })).collect::<Vec<_>>().join(";")
    };
    format!(
        ":root{{{};color-scheme:light dark}}@media(prefers-color-scheme:dark){{:root{{{}}}}}{}{}",
        vars(false),
        vars(true),
        include_str!("../../web/style.css"),
        chart_css(|n| format!("var(--{n})")),
    )
}

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

fn s(f: impl FnOnce(&mut String)) -> String {
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
    pub embed: bool,
}

pub fn layout(app: &App, h: &Head, body: &str) -> String {
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
            r#"<meta property="og:image" content="{img}"><meta property="og:image:width" content="1200"><meta property="og:image:height" content="630"><meta property="og:image:alt" content="{title}"><meta name="twitter:card" content="summary_large_image"><meta name="twitter:image" content="{img}">"#
        );
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
            r#"<header class="top"><div class="wrap"><a class="logo" href="/" aria-label="{SITE} home">{LOGO}<span>plain<b>screener</b></span></a>{search}<a class="gh" href="{REPO}">GitHub ↗</a></div></header>"#,
            search = SEARCH_FORM,
        )
    };
    // While the indexer replays history, say so instead of passing old data off as live.
    let head = app.sh.state.read().map(|s| s.head_ts).unwrap_or(0);
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
            r#"<footer><div class="wrap">Every Base DEX pool, indexed straight from the chain. <a href="{REPO}">Open source</a>. Not financial advice.</div></footer>"#
        )
    };
    format!(
        r##"<!doctype html><html lang="en"{cls}><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><meta name="description" content="{desc}"><link rel="canonical" href="{url}"><link rel="icon" href="/favicon.svg" type="image/svg+xml"><link rel="apple-touch-icon" href="/icon.png"><meta name="theme-color" content="#000000" media="(prefers-color-scheme: dark)"><meta name="theme-color" content="#fafafa" media="(prefers-color-scheme: light)">{meta}<style>{css}</style></head><body>{header}{body}{footer}<script type="module" src="{js}"></script></body></html>"##,
        cls = if h.embed { r#" class="embed""# } else { "" },
        css = app.css,
        js = app.js_url,
    )
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

fn dir(x: f64) -> &'static str {
    if x > 0.0 {
        "u"
    } else if x < 0.0 {
        "d"
    } else {
        "m"
    }
}

/// Live fields of a pair page: (id, text, class). Used for the first render and for updates.
pub fn fields(st: &State, p: u32) -> Vec<(String, String, &'static str)> {
    let m = metrics(st, p);
    let pool = &st.pools[p as usize];
    let mut out = vec![
        ("p".into(), s(|o| fmt::usd(o, m.price)), ""),
        ("pn".into(), s(|o| {
            fmt::price(o, m.native);
            o.push(' ');
            o.push_str(sym(st, pool.quote()));
        }), ""),
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
    out
}

/// A live field as HTML. `fields` holds plain text (live updates set `textContent`),
/// so it's escaped here: the quote symbol in `pn` is anyone's to choose.
fn field(fs: &[(String, String, &'static str)], id: &str) -> String {
    fs.iter()
        .find(|(i, ..)| i == id)
        .map(|(i, text, cls)| format!(r#"<span data-f="{i}" class="{cls}">{}</span>"#, Esc(text)))
        .unwrap_or_default()
}

pub fn trade_row(st: &State, pool: &Pool, t: &Trade) -> String {
    let cls = if t.buy { "u" } else { "d" };
    format!(
        r#"<tr><td class="l m">{time}</td><td class="l {cls}">{kind}</td><td class="{cls}">{price}</td><td>{amount}</td><td>{total}</td><td class="hide-s"><a class="mono" href="{EXPLORER}/address/{maker}" rel="nofollow">{maker_s}</a></td><td class="hide-s"><a class="mono" href="{EXPLORER}/tx/{tx}" rel="nofollow">{tx_s}</a></td></tr>"#,
        time = s(|o| fmt::date_clock(o, t.ts as i64)),
        kind = if t.buy { "Buy" } else { "Sell" },
        price = s(|o| fmt::usd(o, t.price_usd as f64)),
        amount = s(|o| {
            fmt::compact(o, t.base);
            o.push(' ');
            let _ = write!(o, "{}", Esc(sym(st, pool.base())));
        }),
        total = s(|o| fmt::usd_compact(o, t.usd as f64)),
        maker = hex(&t.maker),
        maker_s = short(&t.maker),
        tx = hex(&t.tx),
        tx_s = &hex(&t.tx)[..10],
    )
}

const SEARCH_FORM: &str = r#"<form action="/search" role="search"><input type="search" name="q" placeholder="Search token, pair or address" aria-label="Search tokens, pairs and addresses" autocomplete="off" enterkeyhint="search"></form>"#;

/// A round badge with the token's first letter, its color derived from the address
/// (we don't host logos, and anyone can name a token anything).
fn avatar(st: &State, token: u32, large: bool) -> String {
    let t = &st.tokens[token as usize];
    let hue = (u16::from_be_bytes([t.addr[18], t.addr[19]]) % 360) as u32;
    let letter: String = t.symbol.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
    format!(r#"<span class="av{}" style="--h:{hue}" aria-hidden="true">{}</span>"#, if large { " lg" } else { "" }, Esc(&letter))
}

fn pair_name(st: &State, pool: &Pool, dex: bool) -> String {
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Trending,
    Top,
    Gainers,
    New,
}

pub const TABS: [(Tab, &str, &str); 4] = [
    (Tab::Trending, "trending", "Trending"),
    (Tab::Top, "top", "Top volume"),
    (Tab::Gainers, "gainers", "Gainers"),
    (Tab::New, "new", "New pairs"),
];

/// Ranked pools for a home tab.
pub fn list(st: &State, tab: Tab) -> Vec<u32> {
    let head = st.head_ts;
    let mut rows: Vec<(u32, f64)> = st
        .stats
        .keys()
        .filter(|&&p| st.pools[p as usize].trusted())
        .filter_map(|&p| {
            let m = metrics(st, p);
            let pool = &st.pools[p as usize];
            let score = match tab {
                Tab::Trending if m.liq >= 10_000.0 => m.w[1].vol,
                Tab::Top if m.liq >= 10_000.0 => m.w[3].vol,
                Tab::Gainers if m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0 => change(&m, 3),
                Tab::New if m.liq >= 5_000.0 && pool.created_ts + 3 * 86_400 > head => pool.created_ts as f64,
                _ => return None,
            };
            (score.is_finite() && score > 0.0).then_some((p, score))
        })
        .collect();
    rows.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    rows.truncate(100);
    rows.into_iter().map(|(p, _)| p).collect()
}

fn table(st: &State, pools: &[u32]) -> String {
    if pools.is_empty() {
        return r#"<p class="empty">Nothing here yet. The indexer may still be catching up.</p>"#.into();
    }
    let mut rows = String::new();
    for (i, &p) in pools.iter().enumerate() {
        let pool = &st.pools[p as usize];
        let m = metrics(st, p);
        let txns = m.w[3].buys + m.w[3].sells;
        let _ = write!(
            rows,
            r#"<tr><td class="n">{n}</td><td class="l"><a href="/base/{key}">{name}</a></td><td>{price}</td>"#,
            n = i + 1,
            key = pool.key(),
            name = pair_name(st, pool, true),
            price = s(|o| fmt::usd(o, m.price)),
        );
        for w in 0..4 {
            let c = change(&m, w);
            let hide = if w < 3 { " hide-s" } else { "" };
            let _ = write!(rows, r#"<td class="{}{hide}">{}</td>"#, dir(c), s(|o| fmt::pct(o, c)));
        }
        let _ = write!(
            rows,
            r#"<td>{vol}</td><td>{liq}</td><td class="hide-s">{fdv}</td><td class="hide-s">{txns}</td><td class="hide-s m">{age}</td></tr>"#,
            vol = s(|o| fmt::usd_compact(o, m.w[3].vol)),
            liq = s(|o| fmt::usd_compact(o, m.liq)),
            fdv = s(|o| fmt::usd_compact(o, m.fdv)),
            txns = s(|o| fmt::compact(o, txns as f64)),
            age = s(|o| fmt::age(o, st.head_ts as i64 - pool.created_ts as i64)),
        );
    }
    format!(
        r#"<div class="card scroll"><table class="list"><thead><tr><th>#</th><th class="l">Pair</th><th>Price</th><th class="hide-s">5m</th><th class="hide-s">1h</th><th class="hide-s">6h</th><th>24h</th><th>Volume</th><th>Liquidity</th><th class="hide-s">FDV</th><th class="hide-s">Txns</th><th class="hide-s">Age</th></tr></thead><tbody>{rows}</tbody></table></div>"#
    )
}

pub fn home(app: &App, st: &State, tab: Tab, pools: &[u32]) -> String {
    let tabs: String = TABS
        .iter()
        .map(|(t, slug, label)| {
            let href = if *t == Tab::Trending { "/".to_string() } else { format!("/?tab={slug}") };
            let cur = if *t == tab { r#" aria-current="page""# } else { "" };
            format!(r#"<a href="{href}"{cur}>{label}</a>"#)
        })
        .collect();
    let body = format!(
        r#"<section class="hero"><div class="wrap"><h1>Every token on Base, live.</h1><p>Prices, charts and trades from every Uniswap, Aerodrome and PancakeSwap pool on Base, indexed straight from the chain. Free, fast and open source.</p>{SEARCH_FORM}<div class="links">Embed any chart · <a href="{REPO}">Read the code ↗</a></div></div></section><main class="wrap"><nav class="tabs" aria-label="Lists">{tabs}</nav>{}</main>"#,
        table(st, pools)
    );
    let path = match tab {
        Tab::Trending => "/".to_string(),
        t => format!("/?tab={}", TABS.iter().find(|x| x.0 == t).map(|x| x.1).unwrap_or("")),
    };
    let head = Head {
        title: format!("{SITE}: live prices and charts for every token on Base"),
        desc: "Fast, free and open-source DEX screener for Base. Live charts, trades, liquidity and volume for every Uniswap, Aerodrome and PancakeSwap pool.".into(),
        path,
        image: Some(format!("{}/og.png", app.cfg.public_url)),
        miniapp: None,
        embed: false,
    };
    layout(app, &head, &body)
}

pub fn search(app: &App, st: &State, q: &str, pools: &[u32]) -> String {
    let body = format!(
        r#"<main class="wrap"><h1>Results for “{}”</h1><p class="sub">{} pairs, most liquid first.</p>{}</main>"#,
        Esc(q),
        pools.len(),
        table(st, pools)
    );
    let head = Head {
        title: format!("{} on Base | {SITE}", q),
        desc: format!("Base pairs matching {q}: live price, chart, liquidity and volume."),
        path: format!("/search?q={}", urlencode(q)),
        image: None,
        miniapp: None,
        embed: false,
    };
    layout(app, &head, &body)
}

pub fn not_found(app: &App) -> String {
    let head = Head {
        title: format!("Not found | {SITE}"),
        desc: "This pair or token isn't indexed.".into(),
        path: "/404".into(),
        image: None,
        miniapp: None,
        embed: false,
    };
    layout(
        app,
        &head,
        r#"<main class="wrap"><h1>Not found</h1><p class="sub">We don't index a Base pair or token at that address. Try the search box.</p></main>"#,
    )
}

/// Candles of a pool for the chart, in USD when the quote token has a price, else native.
pub fn chart_data(st: &State, store: &Store, p: u32, ri: usize, before: u32, limit: usize) -> Vec<chart::Candle> {
    let raw = store.candles(p, ri, 0, before, limit);
    let usd = raw.last().is_some_and(|(_, c)| c.c.is_finite()) && st.pools[p as usize].price_usd.is_finite();
    raw.iter()
        .map(|(t, c)| {
            let (o, h, l, cl) = if usd { (c.o, c.h, c.l, c.c) } else { (c.no, c.nh, c.nl, c.nc) };
            chart::Candle { t: *t as f64, o: o as f64, h: h as f64, l: l as f64, c: cl as f64, v: c.v as f64 }
        })
        .filter(|c| c.o.is_finite() && c.c.is_finite())
        .collect()
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
    let fs = fields(st, p);
    let f = |id: &str| field(&fs, id);
    let candles = chart_data(st, store, p, ri, u32::MAX, CHART_CANDLES);
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
        r#"<div class="card chartcard"><nav class="res" aria-label="Candle size">{res_nav}</nav><div id="chart" data-pool="{key}" data-res="{res}" data-wasm="{wasm}">{svg}</div></div><script id="cd" type="application/json">{json}</script>"#,
        res = RES[ri],
        wasm = app.wasm_url,
        json = candles_json(&candles),
    );
    let price_head = format!(r#"<span class="big">{}</span> <span class="chg">{}</span>"#, f("p"), f("c24h"));
    let url = format!("{}/base/{key}", app.cfg.public_url);
    let m = metrics(st, p);
    let price_text = price_label(st, p, &m);
    let title = format!("{bs} {price_text} {} | {bs}/{qs} on Base | {SITE}", s(|o| fmt::pct(o, change(&m, 3))));
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
        embed,
    };

    if embed {
        let body = format!(
            r#"<div class="eh">{name}{price_head}<a class="brand" href="{url}" target="_blank" rel="noopener">{SITE}</a></div>{chart}"#,
            name = pair_name(st, pool, false),
        );
        return layout(app, &head, &body);
    }

    let trades = store.trades(p, (u32::MAX, u32::MAX), 50);
    let rows: String = trades.iter().map(|(_, t)| trade_row(st, pool, t)).collect();
    let last = trades.first().map(|(k, _)| format!("{}-{}", k.0, k.1)).unwrap_or_else(|| "0-0".into());
    let windows: String = WINDOWS
        .iter()
        .map(|(n, _)| {
            format!(
                r#"<tr><td class="l">{n}</td><td>{}</td><td>{}</td><td class="u">{}</td><td class="d">{}</td></tr>"#,
                f(&format!("c{n}")),
                f(&format!("v{n}")),
                f(&format!("b{n}")),
                f(&format!("s{n}"))
            )
        })
        .collect();
    let copy = |label: &str, a: &str, link: &str| {
        format!(
            r#"<p><span>{label}</span><span><a class="mono" href="{EXPLORER}/{link}" rel="nofollow">{short}</a> <button type="button" data-copy="{a}" aria-label="Copy {label} address">Copy</button></span></p>"#,
            short = if a.len() > 12 { format!("{}…{}", &a[..6], &a[a.len() - 4..]) } else { a.to_string() },
        )
    };
    let pool_link = if pool.is_address() { format!("address/{key}") } else { format!("address/{}", hex(&crate::abi::V4_POOL_MANAGER)) };
    let embed_code = format!(
        r#"<iframe src="{}/embed/base/{key}" width="100%" height="480" style="border:0;border-radius:12px" title="{} chart on {SITE}" loading="lazy"></iframe>"#,
        app.cfg.public_url,
        Esc(&format!("{bs}/{qs}")),
    );
    let share = format!(
        "https://x.com/intent/post?text={}&url={}",
        urlencode(&format!("{bs} on Base")),
        urlencode(&format!("{url}?s={}", st.head_ts / 3600))
    );
    let body = format!(
        r#"<main class="wrap"><div class="pair"><section><div class="head"><div class="id">{av}<div class="meta"><h1>{bs_e}<span class="q">/{qs_e}</span></h1><span class="name">{bname} <span class="badge">{dex}</span></span></div></div><div class="px"><div><div class="big">{p}</div><div class="pn">{pn}</div></div><span class="chg">{c24}</span></div></div>{chart}<div class="card scroll"><h2>Trades</h2><table class="trades"><thead><tr><th class="l">Time (UTC)</th><th class="l">Type</th><th>Price</th><th>Amount</th><th>Total</th><th class="hide-s">Maker</th><th class="hide-s">Tx</th></tr></thead><tbody id="tr" data-last="{last}">{rows}</tbody></table></div></section><aside><dl class="card kv"><div><dt>Price USD</dt><dd>{p}</dd></div><div><dt>Price {qs_e}</dt><dd>{pn}</dd></div><div><dt>Liquidity</dt><dd>{liq}</dd></div><div><dt>FDV</dt><dd>{fdv}</dd></div></dl><div class="card scroll"><table class="win"><thead><tr><th class="l">Window</th><th>Change</th><th>Volume</th><th>Buys</th><th>Sells</th></tr></thead><tbody>{windows}</tbody></table></div><div class="card info">{pair_c}{base_c}{quote_c}<p><span>DEX</span><span>{dex}</span></p><p><span>Created</span><span>{created} ago</span></p></div><div class="card share"><h2>Embed or share</h2><textarea readonly aria-label="Embed code">{embed_e}</textarea><div class="row"><button type="button" class="btn-primary" data-copy="{embed_e}">Copy embed code</button><a class="btn" href="{share}" target="_blank" rel="noopener">Share on X ↗</a></div></div></aside></div></main>"#,
        av = avatar(st, pool.base(), true),
        bs_e = Esc(bs),
        c24 = f("c24h"),
        dex = pool.dex(),
        bname = Esc(&base.name),
        pn = f("pn"),
        p = f("p"),
        liq = f("liq"),
        fdv = f("fdv"),
        qs_e = Esc(qs),
        pair_c = copy("Pair", &key, &pool_link),
        base_c = copy(&format!("{}", Esc(bs)), &hex(&base.addr), &format!("token/{}", hex(&base.addr))),
        quote_c = copy(&format!("{}", Esc(qs)), &hex(&quote.addr), &format!("token/{}", hex(&quote.addr))),
        created = s(|o| fmt::age(o, st.head_ts as i64 - pool.created_ts as i64)),
        embed_e = Esc(&embed_code),
    );
    layout(app, &head, &body)
}

/// A live update for a pair page: changed fields, the latest candle of every
/// resolution, and new trade rows. Returns the JSON and the newest trade key.
pub fn live(st: &State, store: &Store, p: u32, after: (u32, u32)) -> (String, (u32, u32)) {
    let pool = &st.pools[p as usize];
    let fs: serde_json::Map<String, serde_json::Value> =
        fields(st, p).into_iter().map(|(id, text, cls)| (id, serde_json::json!([text, cls]))).collect();
    let mut candles = serde_json::Map::new();
    for (ri, res) in RES.iter().enumerate() {
        if let Some(c) = chart_data(st, store, p, ri, u32::MAX, 1).last() {
            candles.insert(res.to_string(), serde_json::json!([c.t, c.o, c.h, c.l, c.c, c.v]));
        }
    }
    let trades = if after == (0, 0) { Vec::new() } else { store.trades_after(p, after) };
    let newest = trades.last().map(|(k, _)| *k).unwrap_or(after);
    let rows: String = trades.iter().rev().take(50).map(|(_, t)| trade_row(st, pool, t)).collect();
    (serde_json::json!({"f": fs, "c": candles, "t": rows}).to_string(), newest)
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
    let mut out = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#);
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
pub fn best_pool(st: &State, token: u32) -> Option<u32> {
    let p = st.tokens.get(token as usize)?.best_pool;
    (p != NONE).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_css() {
        assert_eq!(format!("{}", Esc(r#"<b>"x"&'</b>"#)), "&lt;b&gt;&quot;x&quot;&amp;&#39;&lt;/b&gt;");
        let css = page_css();
        assert!(css.contains("--up:#0a7a3d") && css.contains("fill:var(--up)") && !css.contains("{up}"));
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
