//! Pair lists: home tabs, search results and the watchlist. Filters and sorting live in
//! the URL, so a list can be shared and its live refresh (`/api/list?…`) asks for the same.

use crate::pages::{Esc, WINDOWS, change, dir, metrics, pair_name, s, urlencode};
use crate::state::State;
use chart::fmt;
use serde::Deserialize;
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Tab {
    #[default]
    Trending,
    Top,
    Gainers,
    Losers,
    New,
    /// The pairs this browser starred (app.js asks for them by key).
    Watch,
}

pub const TABS: [(Tab, &str, &str); 6] = [
    (Tab::Trending, "trending", "Trending"),
    (Tab::Top, "top", "Top volume"),
    (Tab::Gainers, "gainers", "Gainers"),
    (Tab::Losers, "losers", "Losers"),
    (Tab::New, "new", "New pairs"),
    (Tab::Watch, "watch", "Watchlist"),
];

/// Sortable columns: (query value, header). The 5m–24h ones are price changes.
const COLS: [(&str, &str); 10] = [
    ("price", "Price"),
    ("c5m", "5m"),
    ("c1h", "1h"),
    ("c6h", "6h"),
    ("c24h", "24h"),
    ("vol", "Volume"),
    ("liq", "Liquidity"),
    ("fdv", "FDV"),
    ("txns", "Txns"),
    ("age", "Age"),
];

/// Choices offered in the filter bar: (value, label). 0 means any.
const LIQ: [(u64, &str); 5] =
    [(0, "Any"), (10_000, "$10K+"), (50_000, "$50K+"), (250_000, "$250K+"), (1_000_000, "$1M+")];
const VOL: [(u64, &str); 5] =
    [(0, "Any"), (10_000, "$10K+"), (100_000, "$100K+"), (1_000_000, "$1M+"), (10_000_000, "$10M+")];
const TXNS: [(u64, &str); 4] = [(0, "Any"), (100, "100+"), (1_000, "1K+"), (10_000, "10K+")];
const AGE: [(u64, &str); 6] = [(0, "Any"), (1, "1h"), (6, "6h"), (24, "24h"), (72, "3d"), (168, "7d")];
/// 24 h volume beyond this many times the liquidity is fake prices, not trading: real pairs
/// peak around 100x (a USDT/USDC pool). ponytail: fixed bound, tune if busy pairs vanish.
const MAX_TURNOVER: f64 = 1_000.0;

/// A list as the URL describes it.
#[derive(Deserialize, Default, Clone, PartialEq, Eq, Hash)]
pub struct Filter {
    pub tab: Option<String>,
    /// Window for Trending, Gainers and Losers: 5m, 1h, 6h or 24h.
    pub w: Option<String>,
    /// Minimum liquidity and 24 h volume in USD, minimum 24 h transactions.
    #[serde(default)]
    pub liq: u64,
    #[serde(default)]
    pub vol: u64,
    #[serde(default)]
    pub txns: u64,
    /// Created at most this many hours ago (0: any age).
    #[serde(default)]
    pub age: u64,
    pub sort: Option<String>,
    /// "asc" for ascending; descending otherwise.
    pub dir: Option<String>,
    /// Search results instead of a tab.
    pub q: Option<String>,
    /// Watchlist: pair keys, comma separated.
    pub pairs: Option<String>,
}

impl Filter {
    pub fn tab(&self) -> Tab {
        TABS.iter().find(|t| Some(t.1) == self.tab.as_deref()).map(|t| t.0).unwrap_or_default()
    }

    /// Index into `WINDOWS` for the tab's ranking.
    fn window(&self) -> usize {
        let default = if self.tab() == Tab::Trending { 1 } else { 3 };
        WINDOWS.iter().position(|(n, _)| Some(*n) == self.w.as_deref()).unwrap_or(default)
    }

    /// The query string for this list with some fields replaced: tab, filters and sort only.
    pub fn query(&self, set: &[(&str, &str)]) -> String {
        let num = |n: u64| if n == 0 { None } else { Some(n.to_string()) };
        let mut fields: Vec<(&str, Option<String>)> = vec![
            ("tab", self.tab.clone().filter(|t| t != "trending")),
            ("w", self.w.clone()),
            ("liq", num(self.liq)),
            ("vol", num(self.vol)),
            ("txns", num(self.txns)),
            ("age", num(self.age)),
            ("sort", self.sort.clone()),
            ("dir", self.dir.clone()),
        ];
        for (k, v) in set {
            if let Some(f) = fields.iter_mut().find(|f| f.0 == *k) {
                f.1 = Some(v.to_string()).filter(|v| !v.is_empty() && !(k == &"tab" && v == "trending"));
            }
        }
        let q: Vec<String> = fields.into_iter().filter_map(|(k, v)| Some(format!("{k}={}", urlencode(&v?)))).collect();
        q.join("&")
    }
}

/// Pools for a tab, filtered and sorted as `f` asks; at most 100.
pub fn ranked(st: &State, f: &Filter) -> Vec<u32> {
    let head = st.head_ts;
    let (tab, w) = (f.tab(), f.window());
    let mut rows: Vec<(u32, f64)> = st
        .stats
        .keys()
        .filter(|&&p| st.pools[p as usize].trusted())
        .filter_map(|&p| {
            let m = metrics(st, p);
            let pool = &st.pools[p as usize];
            let age_h = head.saturating_sub(pool.created_ts) as u64 / 3600;
            let txns = (m.w[3].buys + m.w[3].sells) as u64;
            if m.liq < f.liq as f64
                || m.w[3].vol < f.vol as f64
                || m.w[3].vol > MAX_TURNOVER * m.liq
                || txns < f.txns
                || (f.age > 0 && age_h >= f.age)
            {
                return None;
            }
            let score = match tab {
                Tab::Trending if m.liq >= 10_000.0 => m.w[w].vol,
                Tab::Top if m.liq >= 10_000.0 => m.w[3].vol,
                Tab::Gainers | Tab::Losers
                    if m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0 && st.deep_quote(pool.quote()) =>
                {
                    if tab == Tab::Gainers { change(&m, w) } else { -change(&m, w) }
                }
                Tab::New if m.liq >= 5_000.0 && pool.created_ts + 3 * 86_400 > head => pool.created_ts as f64,
                _ => return None,
            };
            (score.is_finite() && score > 0.0).then_some((p, score))
        })
        .collect();
    rows.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    rows.truncate(100);
    let mut pools: Vec<u32> = rows.into_iter().map(|(p, _)| p).collect();
    sort(st, f, &mut pools);
    pools
}

/// Re-sorts `pools` by the column `f` names, if any. Missing values go last, and so does a
/// price, FDV or price change in a quote token with no real depth behind it (see
/// `State::deep_quote`): whoever mints that token sets its USD price, and so all three.
pub fn sort(st: &State, f: &Filter, pools: &mut [u32]) {
    let Some(col) = f.sort.as_deref().filter(|c| COLS.iter().any(|(k, _)| k == c)) else { return };
    let asc = f.dir.as_deref() == Some("asc");
    let value = |p: u32| -> f64 {
        if matches!(col, "price" | "c5m" | "c1h" | "c6h" | "c24h" | "fdv")
            && !st.deep_quote(st.pools[p as usize].quote())
        {
            return f64::NAN;
        }
        let m = metrics(st, p);
        match col {
            "price" => m.price,
            "c5m" => change(&m, 0),
            "c1h" => change(&m, 1),
            "c6h" => change(&m, 2),
            "c24h" => change(&m, 3),
            "vol" => m.w[3].vol,
            "liq" => m.liq,
            "fdv" => m.fdv,
            "txns" => (m.w[3].buys + m.w[3].sells) as f64,
            _ => st.head_ts.saturating_sub(st.pools[p as usize].created_ts) as f64,
        }
    };
    let mut keyed: Vec<(f64, u32)> = pools.iter().map(|&p| (value(p), p)).collect();
    keyed.sort_by(|a, b| match (a.0.is_finite(), b.0.is_finite()) {
        (true, true) if asc => a.0.total_cmp(&b.0),
        (true, true) => b.0.total_cmp(&a.0),
        (x, y) => y.cmp(&x),
    });
    for (slot, (_, p)) in pools.iter_mut().zip(keyed) {
        *slot = p;
    }
}

/// The rows of a pair list, each with a watchlist star.
pub fn rows(st: &State, pools: &[u32]) -> String {
    let mut rows = String::new();
    for (i, &p) in pools.iter().enumerate() {
        let pool = &st.pools[p as usize];
        let m = metrics(st, p);
        let txns = m.w[3].buys + m.w[3].sells;
        let key = pool.key();
        let (vol, liq, c24) =
            (s(|o| fmt::usd_compact(o, m.w[3].vol)), s(|o| fmt::usd_compact(o, m.liq)), change(&m, 3));
        // Phones get two-line cells instead of the columns they hide: liquidity and volume
        // under the pair, the 24 h change under the price.
        let _ = write!(
            rows,
            r#"<tr><td class="n"><button type="button" class="ic star" data-star="{key}" aria-pressed="false" aria-label="Add to watchlist" title="Watchlist">{star}</button><span class="hide-s">{n}</span></td><td class="l"><a href="/base/{key}">{name}</a><small class="ph">Liq {liq} · Vol {vol}</small></td><td>{price}<small class="ph {cls}">{pct}</small></td>"#,
            n = i + 1,
            star = crate::pages::icon("star"),
            name = pair_name(st, pool, true),
            price = s(|o| fmt::usd(o, m.price)),
            cls = dir(c24),
            pct = s(|o| fmt::pct(o, c24)),
        );
        for w in 0..4 {
            let c = change(&m, w);
            let _ = write!(rows, r#"<td class="{} hide-s">{}</td>"#, dir(c), s(|o| fmt::pct(o, c)));
        }
        let _ = write!(
            rows,
            r#"<td class="hide-s">{vol}</td><td class="hide-s">{liq}</td><td class="hide-s">{fdv}</td><td class="hide-s">{txns}</td><td class="hide-s m">{age}</td></tr>"#,
            fdv = s(|o| fmt::usd_compact(o, m.fdv)),
            txns = s(|o| fmt::compact(o, txns as f64)),
            age = s(|o| fmt::age(o, st.head_ts as i64 - pool.created_ts as i64)),
        );
    }
    rows
}

/// A pair list. With `f`, headers sort it (links back to the home page) and `src` serves
/// fresh rows for app.js; `empty` is shown when there are none.
pub fn table(st: &State, pools: &[u32], f: Option<&Filter>, src: &str, empty: &str) -> String {
    let mut head = String::from(r#"<th><span class="sr">Watch</span>#</th><th class="l">Pair</th>"#);
    for (i, (key, label)) in COLS.iter().enumerate() {
        let cls = match i {
            1..=9 => " hide-s",
            _ => "",
        };
        let Some(f) = f else {
            let _ = write!(head, r#"<th class="{}">{label}</th>"#, cls.trim());
            continue;
        };
        let current = f.sort.as_deref() == Some(*key);
        let asc = f.dir.as_deref() == Some("asc");
        // A click sorts by the column, descending first; clicking again flips it.
        let next = if current && !asc { "asc" } else { "desc" };
        let (aria, arrow) = match (current, asc) {
            (true, true) => (r#" aria-sort="ascending""#, " ↑"),
            (true, false) => (r#" aria-sort="descending""#, " ↓"),
            _ => ("", ""),
        };
        let _ = write!(
            head,
            r#"<th class="{}"{aria}><a href="/?{}" rel="nofollow">{label}{arrow}</a></th>"#,
            cls.trim(),
            Esc(&f.query(&[("sort", key), ("dir", next)])),
        );
    }
    let body = if pools.is_empty() {
        format!(r#"<tr><td colspan="12" class="empty">{empty}</td></tr>"#)
    } else {
        rows(st, pools)
    };
    format!(
        r#"<div class="card scroll" data-list="{src}"><table class="list"><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table></div>"#
    )
}

/// Tabs, the ranking window and a Filters menu above the home list. The menu's form keeps
/// the tab, window and sort.
pub fn controls(f: &Filter) -> String {
    let tab = f.tab();
    let tabs: String = TABS
        .iter()
        .map(|(t, slug, label)| {
            let cur = if *t == tab { r#" aria-current="page""# } else { "" };
            let star = if *t == Tab::Watch { crate::pages::icon("star") } else { String::new() };
            format!(
                r#"<a href="/?{}"{cur}>{star}{label}</a>"#,
                Esc(&Filter { tab: Some(slug.to_string()), ..Default::default() }.query(&[]))
            )
        })
        .collect();
    let tabs = format!(r#"<nav class="tabs" aria-label="Lists">{tabs}</nav>"#);
    if tab == Tab::Watch {
        return format!(r#"<div class="listbar">{tabs}</div>"#);
    }
    let window = if matches!(tab, Tab::Trending | Tab::Gainers | Tab::Losers) {
        let links: String = WINDOWS
            .iter()
            .enumerate()
            .map(|(i, (n, _))| {
                let cur = if i == f.window() { r#" aria-current="true""# } else { "" };
                format!(r#"<a href="/?{}" rel="nofollow"{cur}>{n}</a>"#, Esc(&f.query(&[("w", n)])))
            })
            .collect();
        format!(r#"<nav class="res" aria-label="Window">{links}</nav>"#)
    } else {
        String::new()
    };
    let select = |name: &str, label: &str, opts: &[(u64, &str)], cur: u64| {
        let o: String = opts
            .iter()
            .map(|(v, l)| format!(r#"<option value="{v}"{}>{l}</option>"#, if *v == cur { " selected" } else { "" }))
            .collect();
        format!(r#"<label>{label}<select name="{name}">{o}</select></label>"#)
    };
    let keep: String = [("tab", &f.tab), ("w", &f.w), ("sort", &f.sort), ("dir", &f.dir)]
        .iter()
        .filter_map(|(k, v)| Some(format!(r#"<input type="hidden" name="{k}" value="{}">"#, Esc(v.as_deref()?))))
        .collect();
    let active = [f.liq, f.vol, f.txns, f.age].iter().filter(|v| **v > 0).count();
    let count = if active > 0 { format!(r#"<b class="cnt">{active}</b>"#) } else { String::new() };
    let reset =
        Filter { tab: f.tab.clone(), w: f.w.clone(), sort: f.sort.clone(), dir: f.dir.clone(), ..Default::default() };
    format!(
        r#"<div class="listbar">{tabs}<div class="lb-r">{window}<button type="button" class="btn" popovertarget="filters">{funnel}Filters{count}</button></div><form id="filters" class="menu filters" action="/" popover aria-label="Filters"><p>Filters</p>{keep}{}{}{}{}<div class="row"><a class="btn" href="/?{}">Reset</a><button type="submit" class="btn btn-primary">Apply</button></div></form></div>"#,
        select("liq", "Liquidity", &LIQ, f.liq),
        select("vol", "Volume 24h", &VOL, f.vol),
        select("txns", "Txns 24h", &TXNS, f.txns),
        select("age", "Age under", &AGE, f.age),
        Esc(&reset.query(&[])),
        funnel = crate::pages::icon("funnel"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings() {
        let f = Filter { tab: Some("gainers".into()), liq: 50_000, sort: Some("vol".into()), ..Default::default() };
        assert_eq!(f.query(&[]), "tab=gainers&liq=50000&sort=vol");
        assert_eq!(f.query(&[("sort", "liq"), ("dir", "asc")]), "tab=gainers&liq=50000&sort=liq&dir=asc");
        assert_eq!(Filter::default().query(&[("tab", "trending")]), "");
        assert_eq!(f.tab(), Tab::Gainers);
        assert_eq!(f.window(), 3);
    }
}
