//! Routes. Pages are rendered per request (microseconds) and cached by Cloudflare for a
//! few seconds; live data goes over SSE.

use crate::abi::{pad, unhex};
use crate::model::{RES, res_index};
use crate::pages::{self, Tab};
use crate::state::State;
use crate::{App, og};
use axum::extract::{Path, Query, State as Ax};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
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
        .route("/og/base/{file}", get(og_pair))
        .route("/og.png", get(|| async { png_static(&SITE_PNG) }))
        .route("/icon.png", get(|| async { png_static(&ICON_PNG) }))
        .route("/favicon.svg", get(|| async { cached("image/svg+xml", 86_400, pages::LOGO) }))
        .route("/favicon.ico", get(|| async { Redirect::permanent("/favicon.svg") }))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/a/{file}", get(asset))
        .route("/api/candles/{id}", get(candles))
        .route("/api/live/{id}", get(live))
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

fn cached(ctype: &'static str, secs: u32, body: impl IntoResponse) -> Response {
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
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    r
}

fn read(app: &App) -> std::sync::RwLockReadGuard<'_, State> {
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

fn pool_of(st: &State, id: &str) -> Option<u32> {
    match resolve(st, id)? {
        Found::Pool(p) => Some(p),
        Found::Token(t) => pages::best_pool(st, t),
    }
}

#[derive(Deserialize)]
struct HomeQ {
    tab: Option<String>,
}

async fn home(Ax(app): A, Query(q): Query<HomeQ>) -> Response {
    let tab = pages::TABS.iter().find(|t| Some(t.1) == q.tab.as_deref()).map(|t| t.0).unwrap_or(Tab::Trending);
    let pools = app.list(tab);
    html(pages::home(&app, &read(&app), tab, &pools), 5)
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
    let needle = q.to_lowercase();
    let index = app.search.read().unwrap_or_else(|e| e.into_inner());
    let mut hits: Vec<(bool, f64, u32)> = index
        .iter()
        .filter(|(sym, name, _)| sym.starts_with(&needle) || name.contains(&needle))
        .filter_map(|(sym, _, t)| pages::best_pool(&st, *t).map(|p| (*sym == needle, st.liq(p), p)))
        .collect();
    hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
    let mut pools: Vec<u32> = hits.into_iter().map(|h| h.2).collect();
    pools.dedup();
    pools.truncate(50);
    html(pages::search(&app, &st, q, &pools), 30)
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
        Found::Token(t) => match pages::best_pool(&st, t) {
            Some(p) => {
                let prefix = if embed { "/embed/base" } else { "/base" };
                return Redirect::to(&format!("{prefix}/{}", st.pools[p as usize].key())).into_response();
            }
            None => return (StatusCode::NOT_FOUND, html(pages::not_found(&app), 60)).into_response(),
        },
    };
    let ri = q.res.as_deref().and_then(res_index).unwrap_or(2);
    html(pages::pair(&app, &st, &app.sh.store, p, ri, embed), 3)
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
        let Some(p) = pool_of(&st, id) else { return StatusCode::NOT_FOUND.into_response() };
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

static SITE_PNG: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| og::png(&og::site_card()).unwrap_or_default());
static ICON_PNG: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| og::png(&og::icon()).unwrap_or_default());

fn png_static(bytes: &'static [u8]) -> Response {
    cached("image/png", 86_400, bytes)
}

async fn robots(Ax(app): A) -> Response {
    let body = format!("User-agent: *\nAllow: /\nDisallow: /api/\nDisallow: /embed/\nSitemap: {}/sitemap.xml\n", app.cfg.public_url);
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

#[derive(Deserialize)]
struct CandlesQ {
    res: Option<u32>,
    before: Option<u32>,
}

async fn candles(Ax(app): A, Path(id): Path<String>, Query(q): Query<CandlesQ>) -> Response {
    let st = read(&app);
    let Some(p) = pool_of(&st, &id) else { return StatusCode::NOT_FOUND.into_response() };
    let ri = RES.iter().position(|&r| Some(r) == q.res).unwrap_or(2);
    let data = pages::chart_data(&st, &app.sh.store, p, ri, q.before.unwrap_or(u32::MAX), pages::CHART_CANDLES);
    let edge = if q.before.is_some() { 3600 } else { 3 };
    let mut r = cached("application/json", 0, pages::candles_json(&data));
    if let Ok(v) = HeaderValue::from_str(&format!("public, max-age=0, s-maxage={edge}")) {
        r.headers_mut().insert(header::CACHE_CONTROL, v);
    }
    r
}

#[derive(Deserialize)]
struct LiveQ {
    after: Option<String>,
}

async fn live(Ax(app): A, Path(id): Path<String>, Query(q): Query<LiveQ>) -> Response {
    let Some(p) = pool_of(&read(&app), &id) else { return StatusCode::NOT_FOUND.into_response() };
    let mut after = q
        .after
        .as_deref()
        .and_then(|a| a.split_once('-'))
        .and_then(|(b, i)| Some((b.parse().ok()?, i.parse().ok()?)))
        .unwrap_or((0, 0));
    let stream = BroadcastStream::new(app.sh.live.subscribe()).filter_map(move |msg| {
        let touched = msg.ok()?;
        if !touched.contains(&p) {
            return None;
        }
        let (json, newest) = pages::live(&read(&app), &app.sh.store, p, after);
        after = newest;
        Some(Ok::<_, Infallible>(Event::default().data(json)))
    });
    let mut r = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    r
}

async fn health(Ax(app): A) -> Response {
    let st = read(&app);
    let lag = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
        - st.head_ts as i64;
    let body = format!("head_ts {} lag {lag}s pools {} tokens {}\n", st.head_ts, st.pools.len(), st.tokens.len());
    let code = if lag < 120 { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    let mut r = (code, body).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Short-lived caches for things computed from the whole state.
#[derive(Default)]
pub struct Lists(pub std::sync::Mutex<HashMap<Tab, (Instant, Vec<u32>)>>);

impl App {
    fn list(&self, tab: Tab) -> Vec<u32> {
        let mut cache = self.lists.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = cache.get(&tab)
            && at.elapsed() < Duration::from_secs(3)
        {
            return v.clone();
        }
        let v = pages::list(&read(self), tab);
        cache.insert(tab, (Instant::now(), v.clone()));
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
