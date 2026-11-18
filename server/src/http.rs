//! Routes. Pages are rendered per request (microseconds) and cached by Cloudflare for a
//! few seconds; live data goes over SSE.

use crate::abi::{hex, pad, unhex};
use crate::lists::{self, Filter, Tab};
use crate::model::{RES, res_index};
use crate::pages;
use crate::state::State;
use crate::{App, og};
use axum::Router;
use axum::extract::{Path, Query, State as Ax};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

type A = Ax<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/search", get(search))
        .route("/base/{id}", get(pair))
        .route("/embed/base/{id}", get(embed))
        .route("/base/token/{addr}", get(token))
        .route("/og/base/{file}", get(og_pair))
        .route("/og.png", get(|| async { png_static(&SITE_PNG) }))
        .route("/icon.png", get(|| async { png_static(&ICON_PNG) }))
        .route("/favicon.svg", get(|| async { cached("image/svg+xml", 86_400, pages::LOGO) }))
        .route("/favicon.ico", get(|| async { Redirect::permanent("/favicon.svg") }))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/a/{file}", get(asset))
        .route("/i/{file}", get(icon))
        .route("/api/candles/{id}", get(candles))
        .route("/api/live/{id}", get(live))
        .route("/api/trades/{id}", get(trades))
        .route("/api/list", get(list))
        .nest("/api/v1", crate::api::router())
        .route("/api", get(api_docs))
        .route("/api/traders/{id}", get(traders))
        .route("/healthz", get(health))
        .fallback(|Ax(app): A| async move { (StatusCode::NOT_FOUND, html(pages::not_found(&app), 60)) })
        .layer(axum::middleware::from_fn(no_www))
        .with_state(app)
}

async fn no_www(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
    if let Some(apex) = host.strip_prefix("www.") {
        let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        return Redirect::permanent(&format!("https://{apex}{path}")).into_response();
    }
    next.run(req).await
}

pub(crate) fn cached(ctype: &'static str, secs: u32, body: impl IntoResponse) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    if let Ok(v) = HeaderValue::from_str(&format!("public, max-age={secs}")) {
        h.insert(header::CACHE_CONTROL, v);
    }
    r
}

/// HTML that browsers revalidate and Cloudflare keeps for `edge` seconds.
fn html(body: String, edge: u32) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&format!("public, max-age=0, s-maxage={edge}, stale-while-revalidate=60")) {
        h.insert(header::CACHE_CONTROL, v);
    }
    if let Ok(v) = HeaderValue::from_str(&pages::CSP) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    r
}

pub(crate) fn read(app: &App) -> std::sync::RwLockReadGuard<'_, State> {
    app.sh.state.read().unwrap_or_else(|e| e.into_inner())
}

enum Found {
    Pool(u32),
    Token(u32),
}

fn resolve(st: &State, id: &str) -> Option<Found> {
    let bytes = unhex(id.trim())?;
    match bytes.len() {
        20 => {
            let a: [u8; 20] = bytes.try_into().ok()?;
            if let Some(&p) = st.pool_ix.get(&pad(&a)) {
                return Some(Found::Pool(p));
            }
            st.token_ix.get(&a).map(|&t| Found::Token(t))
        }
        32 => st.pool_ix.get(&<[u8; 32]>::try_from(bytes).ok()?).map(|&p| Found::Pool(p)),
        _ => None,
    }
}

pub(crate) fn pool_of(st: &State, id: &str) -> Option<u32> {
    match resolve(st, id)? {
        Found::Pool(p) => Some(p),
        Found::Token(t) => pages::best_pool(st, t),
    }
}

async fn home(Ax(app): A, Query(f): Query<Filter>) -> Response {
    let pools = if f.tab() == Tab::Watch { Vec::new() } else { app.list(&f) };
    html(pages::home(&app, &read(&app), &f, &pools), 15)
}

/// Fresh rows for a home tab, search results or watchlist; pages poll this while open.
async fn list(Ax(app): A, Query(f): Query<Filter>) -> Response {
    let pools = match (f.q.as_deref().map(str::trim), f.pairs.as_deref()) {
        (Some(needle), _) if !needle.is_empty() => search_pools(&app, &read(&app), needle),
        (_, Some(keys)) => {
            let st = read(&app);
            let mut pools: Vec<u32> = keys.split(',').take(100).filter_map(|k| pool_of(&st, k)).collect();
            lists::sort(&st, &f, &mut pools);
            pools
        }
        _ if f.tab() == Tab::Watch => Vec::new(),
        _ => app.list(&f),
    };
    let mut r = cached("text/html; charset=utf-8", 0, lists::rows(&read(&app), &pools));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, s-maxage=2"));
    r
}

#[derive(Deserialize)]
struct SearchQ {
    q: Option<String>,
}

async fn search(Ax(app): A, Query(q): Query<SearchQ>) -> Response {
    let q = q.q.unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Redirect::to("/").into_response();
    }
    let st = read(&app);
    if let Some(p) = pool_of(&st, q) {
        return Redirect::to(&format!("/base/{}", st.pools[p as usize].key())).into_response();
    }
    let pools = search_pools(&app, &st, q);
    html(pages::search(&app, &st, q, &pools), 30)
}

/// Pools for tokens whose symbol starts with or name contains `q`: exact symbol matches
/// first, then pools from known DEXes, then by liquidity.
pub(crate) fn search_pools(app: &App, st: &State, q: &str) -> Vec<u32> {
    let needle = q.to_lowercase();
    let index = app.search.read().unwrap_or_else(|e| e.into_inner());
    // Exact symbol matches first, then pools from known DEXes, then by liquidity.
    let mut hits: Vec<((bool, bool), f64, u32)> = index
        .iter()
        .filter(|(sym, name, _)| sym.starts_with(&needle) || name.contains(&needle))
        .filter_map(|(sym, _, t)| {
            let p = pages::best_pool(st, *t)?;
            Some(((*sym == needle, st.pools[p as usize].trusted()), st.liq(p), p))
        })
        .collect();
    hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
    let mut pools: Vec<u32> = hits.into_iter().map(|h| h.2).collect();
    pools.dedup();
    pools.truncate(50);
    pools
}

#[derive(Deserialize)]
struct PairQ {
    res: Option<String>,
}

async fn pair(app: A, Path(id): Path<String>, q: Query<PairQ>) -> Response {
    pair_page(app, id, q, false)
}

async fn embed(app: A, Path(id): Path<String>, q: Query<PairQ>) -> Response {
    pair_page(app, id, q, true)
}

fn pair_page(Ax(app): A, id: String, Query(q): Query<PairQ>, embed: bool) -> Response {
    let st = read(&app);
    let Some(found) = resolve(&st, &id) else {
        return (StatusCode::NOT_FOUND, html(pages::not_found(&app), 60)).into_response();
    };
    let p = match found {
        Found::Pool(p) => p,
        // A token's chart is a pool where it's priced; a token only ever quoted (a stablecoin,
        // say) gets its token page instead of some other token's price. Embeds need a chart.
        Found::Token(t) => match (pages::own_pool(&st, t), embed) {
            (Some(p), _) => {
                return Redirect::to(&format!(
                    "{}/{}",
                    if embed { "/embed/base" } else { "/base" },
                    st.pools[p as usize].key()
                ))
                .into_response();
            }
            (None, false) => {
                return Redirect::to(&format!("/base/token/{}", hex(&st.tokens[t as usize].addr))).into_response();
            }
            (None, true) => match pages::best_pool(&st, t) {
                Some(p) => return Redirect::to(&format!("/embed/base/{}", st.pools[p as usize].key())).into_response(),
                None => return (StatusCode::NOT_FOUND, html(pages::not_found(&app), 60)).into_response(),
            },
        },
    };
    let ri = q.res.as_deref().and_then(res_index).unwrap_or_else(|| pages::default_res(&st, p, 2));
    // Cached for a minute at the edge: the live stream brings a stale copy up to date
    // as soon as it connects.
    html(pages::pair(&app, &st, &app.sh.store, p, ri, embed), 60)
}

async fn api_docs(Ax(app): A) -> Response {
    html(pages::api_docs(&app, &read(&app)), 3600)
}

async fn token(Ax(app): A, Path(addr): Path<String>) -> Response {
    let st = read(&app);
    let t = unhex(addr.trim()).and_then(|b| <[u8; 20]>::try_from(b).ok()).and_then(|a| st.token_ix.get(&a).copied());
    match t {
        Some(t) => html(pages::token(&app, &st, &app.sh.store, t), 30),
        None => (StatusCode::NOT_FOUND, html(pages::not_found(&app), 60)).into_response(),
    }
}

#[derive(Deserialize)]
struct OgQ {
    s: Option<String>,
}

async fn og_pair(Ax(app): A, Path(file): Path<String>, Query(q): Query<OgQ>) -> Response {
    let id = file.trim_end_matches(".png");
    let fc = q.s.as_deref() == Some("fc");
    let (p, minute) = {
        let st = read(&app);
        let Some(p) = pool_of(&st, id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (p, st.head_ts / 60)
    };
    if let Some((m, png)) = app.og.lock().unwrap_or_else(|e| e.into_inner()).get(&(p, fc))
        && *m == minute
    {
        return cached("image/png", 300, png.to_vec());
    }
    let app2 = app.clone();
    let png = tokio::task::spawn_blocking(move || {
        let svg = {
            let st = read(&app2);
            og::pair_card(&st, &app2.sh.store, p, 1200, if fc { 800 } else { 630 })
        };
        og::png(&svg)
    })
    .await;
    match png {
        Ok(Ok(png)) => {
            let png = Arc::new(png);
            app.og.lock().unwrap_or_else(|e| e.into_inner()).insert((p, fc), (minute, png.clone()));
            cached("image/png", 300, png.to_vec())
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

static SITE_PNG: std::sync::LazyLock<Vec<u8>> =
    std::sync::LazyLock::new(|| og::png(&og::site_card()).unwrap_or_default());
static ICON_PNG: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| og::png(&og::icon()).unwrap_or_default());

fn png_static(bytes: &'static [u8]) -> Response {
    cached("image/png", 86_400, bytes)
}

async fn robots(Ax(app): A) -> Response {
    let body = format!("User-agent: *\nAllow: /\nDisallow: /api/\nSitemap: {}/sitemap.xml\n", app.cfg.public_url);
    cached("text/plain; charset=utf-8", 3600, body)
}

async fn sitemap(Ax(app): A) -> Response {
    cached("application/xml", 3600, pages::sitemap(&app, &read(&app)))
}

async fn asset(Ax(app): A, Path(file): Path<String>) -> Response {
    let immutable = |ctype, body: &'static [u8]| {
        let mut r = cached(ctype, 31_536_000, body);
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
        r
    };
    if file == app.js_file {
        immutable("text/javascript; charset=utf-8", crate::JS.as_bytes())
    } else if file == app.wasm_file {
        immutable("application/wasm", crate::WASM)
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// A token icon (always a PNG we drew, see `icons`).
async fn icon(Ax(app): A, Path(file): Path<String>) -> Response {
    let addr = file.strip_suffix(".png").and_then(unhex).filter(|a| a.len() == 20);
    match addr.and_then(|a| app.sh.store.icons.get(a).ok().flatten()).filter(|v| v.len() > 4) {
        Some(png) => cached("image/png", 86_400, png.to_vec()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct CandlesQ {
    res: Option<u32>,
    before: Option<u32>,
    /// "n": in the quote token instead of USD.
    unit: Option<String>,
}

async fn candles(Ax(app): A, Path(id): Path<String>, Query(q): Query<CandlesQ>) -> Response {
    let st = read(&app);
    let Some(p) = pool_of(&st, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ri = RES.iter().position(|&r| Some(r) == q.res).unwrap_or(2);
    let data = pages::chart_data(
        &st,
        &app.sh.store,
        p,
        ri,
        q.before.unwrap_or(u32::MAX),
        pages::CHART_CANDLES,
        q.unit.as_deref() == Some("n"),
    );
    let edge = if q.before.is_some() { 3600 } else { 3 };
    let mut r = cached("application/json", 0, pages::candles_json(&data));
    if let Ok(v) = HeaderValue::from_str(&format!("public, max-age=0, s-maxage={edge}")) {
        r.headers_mut().insert(header::CACHE_CONTROL, v);
    }
    r
}

/// "b-i" (block, log index), as in trade cursors and live event ids.
pub(crate) fn trade_cursor(s: &str) -> Option<(u32, u32)> {
    let (b, i) = s.split_once('-')?;
    Some((b.parse().ok()?, i.parse().ok()?))
}

#[derive(Deserialize)]
struct TradesQ {
    before: Option<String>,
    /// Only this wallet's trades.
    maker: Option<String>,
}

async fn trades(Ax(app): A, Path(id): Path<String>, Query(q): Query<TradesQ>) -> Response {
    let st = read(&app);
    let Some(p) = pool_of(&st, &id) else { return StatusCode::NOT_FOUND.into_response() };
    let maker: Option<[u8; 20]> = match q.maker.as_deref() {
        Some(m) => match unhex(m).and_then(|b| b.try_into().ok()) {
            Some(a) => Some(a),
            None => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => None,
    };
    let before = match q.before.as_deref().map(trade_cursor) {
        Some(Some(b)) => b,
        Some(None) => return StatusCode::BAD_REQUEST.into_response(),
        None if maker.is_some() => (u32::MAX, u32::MAX),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let mut r = cached("application/json", 0, pages::trades_page(&st, &app.sh.store, p, before, maker));
    // Older trades never change; a wallet's latest do.
    let cache =
        if q.before.is_some() { "public, max-age=3600, s-maxage=86400" } else { "public, max-age=0, s-maxage=5" };
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    r
}

/// A pool's top traders as a table (see `pages::top_traders`).
async fn traders(Ax(app): A, Path(id): Path<String>) -> Response {
    let st = read(&app);
    let Some(p) = pool_of(&st, &id) else { return StatusCode::NOT_FOUND.into_response() };
    let rows = pages::top_traders(&st, &app.sh.store, p);
    let body = format!(
        r#"<p class="tsub">Profit on what each wallet bought here (last 100K trades), with anything still held at today's price.</p><div class="scroll"><table class="trades tops"><thead><tr><th class="l">#</th><th class="l">Wallet</th><th>Bought</th><th>Sold</th><th>Profit</th><th class="hide-s">Buys / sells</th></tr></thead><tbody>{rows}</tbody></table></div>"#
    );
    let mut r = cached("text/html; charset=utf-8", 0, body);
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, s-maxage=60"));
    r
}

#[derive(Deserialize)]
struct LiveQ {
    after: Option<String>,
}

async fn live(Ax(app): A, Path(id): Path<String>, Query(q): Query<LiveQ>, headers: HeaderMap) -> Response {
    let Some(p) = pool_of(&read(&app), &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // A reconnecting EventSource reports the last trade it got as the last event id.
    let last_id = headers.get("last-event-id").and_then(|v| v.to_str().ok());
    let mut after = last_id.or(q.after.as_deref()).and_then(trade_cursor).unwrap_or((0, 0));
    // Subscribe before the catch-up snapshot so nothing falls between them.
    let rx = app.sh.live.subscribe();
    let (first, newest) = pages::live(&read(&app), &app.sh.store, p, after);
    after = newest;
    let event =
        |json: String, (b, i): (u32, u32)| Ok::<_, Infallible>(Event::default().id(format!("{b}-{i}")).data(json));
    let first = tokio_stream::once(event(first, after));
    let stream = first.chain(BroadcastStream::new(rx).filter_map(move |msg| {
        let touched = msg.ok()?;
        if !touched.contains(&p) {
            return None;
        }
        let (json, newest) = pages::live(&read(&app), &app.sh.store, p, after);
        after = newest;
        Some(event(json, after))
    }));
    let mut r = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    r
}

async fn health(Ax(app): A) -> Response {
    let st = read(&app);
    let lag = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
        - st.head_ts as i64;
    let live = lag < 120;
    let body = format!(
        "status {} head_ts {} lag {lag}s pools {} tokens {}\n",
        if live { "live" } else { "catching-up" },
        st.head_ts,
        st.pools.len(),
        st.tokens.len()
    );
    let code = if live { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    let mut r = (code, body).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Short-lived caches for lists computed from the whole state, by what was asked.
#[derive(Default)]
pub struct Lists(pub std::sync::Mutex<HashMap<Filter, (Instant, Vec<u32>)>>);

impl App {
    pub(crate) fn list(&self, f: &Filter) -> Vec<u32> {
        let mut cache = self.lists.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = cache.get(f)
            && at.elapsed() < Duration::from_secs(3)
        {
            return v.clone();
        }
        let v = lists::ranked(&read(self), f);
        // Filters are free-form; don't let odd combinations pile up.
        if cache.len() > 500 {
            cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3));
        }
        cache.insert(f.clone(), (Instant::now(), v.clone()));
        v
    }
}

/// Rebuilds the search index (tokens with a pool worth showing) every minute.
pub async fn search_index(app: Arc<App>) {
    loop {
        let index: Vec<(String, String, u32)> = {
            let st = read(&app);
            st.tokens
                .iter()
                .enumerate()
                .filter(|(_, t)| t.best_pool != crate::state::NONE && st.liq(t.best_pool) >= 1_000.0)
                .map(|(i, t)| (t.symbol.to_lowercase(), t.name.to_lowercase(), i as u32))
                .collect()
        };
        *app.search.write().unwrap_or_else(|e| e.into_inner()) = index;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
