//! The public JSON API under /api/v1 (documented at /api). Free, no key, CORS open.
//! Responses are cached at Cloudflare's edge for a few seconds; requests that reach us
//! are limited per IP.

use crate::abi::{V4_POOL_MANAGER, hex, unhex};
use crate::http::{cached, pool_of, read, search_pools, trade_cursor};
use crate::lists::{self, Filter, TABS};
use crate::model::{RES, TradeKind};
use crate::pages::{WINDOWS, change, metrics};
use crate::state::{NONE, State};
use crate::store::Store;
use crate::{App, pages};
use axum::extract::{Path, Query, Request, State as Ax};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Requests per minute per IP that reach the server (edge-cached ones don't count).
pub const LIMIT: u32 = 120;

type A = Ax<Arc<App>>;

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/search", get(search))
        .route("/pairs/{id}", get(pair))
        .route("/pairs/{id}/candles", get(candles))
        .route("/pairs/{id}/trades", get(trades))
        .route("/tokens/{addr}", get(token))
        .route("/lists/{tab}", get(list))
        .fallback(|| async { err(StatusCode::NOT_FOUND, "no such endpoint, see /api") })
        .layer(axum::middleware::from_fn(limit))
}

fn err(code: StatusCode, msg: &str) -> Response {
    let mut r = (code, Json(json!({ "error": msg }))).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// JSON cached at the edge for `edge` seconds.
fn ok(v: Value, edge: u32) -> Response {
    let mut r = cached("application/json", 0, v.to_string());
    if let Ok(h) = HeaderValue::from_str(&format!("public, max-age=0, s-maxage={edge}")) {
        r.headers_mut().insert(header::CACHE_CONTROL, h);
    }
    r
}

/// Per-IP counts for the current minute.
static HITS: std::sync::LazyLock<Mutex<(u64, HashMap<String, u32>)>> = std::sync::LazyLock::new(Default::default);

/// Rate limit (by the visitor's IP from Cloudflare) and open CORS.
async fn limit(req: Request, next: Next) -> Response {
    let ip = req.headers().get("cf-connecting-ip").and_then(|v| v.to_str().ok()).unwrap_or("direct").to_string();
    let minute = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    let used = {
        let mut hits = HITS.lock().unwrap_or_else(|e| e.into_inner());
        if hits.0 != minute {
            *hits = (minute, HashMap::new());
        }
        let n = hits.1.entry(ip).or_default();
        *n += 1;
        *n
    };
    let mut r = if used > LIMIT {
        let mut r = err(StatusCode::TOO_MANY_REQUESTS, "rate limited: 120 requests a minute per IP");
        let wait = 60 - SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() % 60).unwrap_or(0);
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
        r
    } else {
        next.run(req).await
    };
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert("x-ratelimit-limit", HeaderValue::from(LIMIT));
    h.insert("x-ratelimit-remaining", HeaderValue::from(LIMIT.saturating_sub(used)));
    r
}

fn num(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

fn token_json(st: &State, t: u32) -> Value {
    let tok = &st.tokens[t as usize];
    json!({
        "address": hex(&tok.addr),
        "symbol": tok.symbol,
        "name": tok.name,
        "decimals": tok.decimals,
        "price_usd": num(tok.usd),
        "icon": tok.icon.then(|| format!("/i/{}.png", hex(&tok.addr))),
    })
}

/// Everything shown about a pair, as JSON.
pub fn pair_json(app: &App, st: &State, p: u32) -> Value {
    let pool = &st.pools[p as usize];
    let m = metrics(st, p);
    let (bb, qb) = if pool.quote0 { (pool.bal1, pool.bal0) } else { (pool.bal0, pool.bal1) };
    let by_window = |f: &dyn Fn(usize) -> Value| -> Value {
        WINDOWS.iter().enumerate().map(|(i, (n, _))| (n.to_string(), f(i))).collect::<serde_json::Map<_, _>>().into()
    };
    json!({
        "pair": pool.key(),
        "url": format!("{}/base/{}", app.cfg.public_url, pool.key()),
        "dex": pool.dex(),
        "trusted": pool.trusted(),
        "contract": hex(if pool.is_address() { &pool.id[12..] } else { &V4_POOL_MANAGER }),
        "base": token_json(st, pool.base()),
        "quote": token_json(st, pool.quote()),
        "price_usd": num(m.price),
        "price_native": num(m.native),
        "liquidity_usd": num(m.liq),
        "fdv_usd": num(m.fdv),
        "pooled": {"base": num(bb), "quote": num(qb)},
        "change": by_window(&|i| num(change(&m, i))),
        "volume_usd": by_window(&|i| num(m.w[i].vol)),
        "txns": by_window(&|i| json!({"buys": m.w[i].buys, "sells": m.w[i].sells})),
        "created_at": pool.created_ts,
        "last_trade_at": pool.last_ts,
    })
}

#[derive(Deserialize)]
struct SearchQ {
    q: Option<String>,
}

async fn search(Ax(app): A, Query(q): Query<SearchQ>) -> Response {
    let q = q.q.unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return err(StatusCode::BAD_REQUEST, "q is required");
    }
    let st = read(&app);
    let pools = match pool_of(&st, q) {
        Some(p) => vec![p],
        None => search_pools(&app, &st, q),
    };
    let pairs: Vec<Value> = pools.iter().take(20).map(|&p| pair_json(&app, &st, p)).collect();
    ok(json!({ "pairs": pairs }), 30)
}

async fn pair(Ax(app): A, Path(id): Path<String>) -> Response {
    let st = read(&app);
    match pool_of(&st, &id) {
        Some(p) => ok(pair_json(&app, &st, p), 3),
        None => err(StatusCode::NOT_FOUND, "unknown pair or token"),
    }
}

#[derive(Deserialize)]
struct CandlesQ {
    res: Option<String>,
    before: Option<u32>,
    limit: Option<usize>,
    unit: Option<String>,
}

async fn candles(Ax(app): A, Path(id): Path<String>, Query(q): Query<CandlesQ>) -> Response {
    let st = read(&app);
    let Some(p) = pool_of(&st, &id) else { return err(StatusCode::NOT_FOUND, "unknown pair or token") };
    let ri = match q.res.as_deref() {
        None => 2,
        Some(r) => match crate::model::res_index(r).or_else(|| RES.iter().position(|&x| r.parse() == Ok(x))) {
            Some(i) => i,
            None => return err(StatusCode::BAD_REQUEST, "res is one of 1m 5m 15m 1h 4h 1d"),
        },
    };
    let native = q.unit.as_deref() == Some("native");
    let limit = q.limit.unwrap_or(300).clamp(1, 1000);
    let cs = pages::chart_data(&st, &app.sh.store, p, ri, q.before.unwrap_or(u32::MAX), limit, native);
    let rows: Vec<Value> = cs.iter().map(|c| json!([c.t, num(c.o), num(c.h), num(c.l), num(c.c), num(c.v)])).collect();
    let unit = if native { "native" } else { "usd" };
    ok(json!({"res": RES[ri], "unit": unit, "candles": rows}), if q.before.is_some() { 3600 } else { 3 })
}

#[derive(Deserialize)]
struct TradesQ {
    before: Option<String>,
    maker: Option<String>,
    limit: Option<usize>,
}

fn trade_json(store: &Store, p: u32, q: &TradesQ) -> Result<Value, &'static str> {
    let before = match q.before.as_deref() {
        None => (u32::MAX, u32::MAX),
        Some(b) => trade_cursor(b).ok_or("before is block-index, e.g. 12345678-9")?,
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let (page, scanned) = match q.maker.as_deref() {
        None => {
            let page = store.trades(p, before, limit);
            let next = (page.len() == limit).then(|| page.last().map(|(k, _)| *k)).flatten();
            (page, next)
        }
        Some(m) => {
            let m: [u8; 20] = unhex(m).and_then(|b| b.try_into().ok()).ok_or("maker is an address")?;
            store.trades_by(p, m, before, limit, 20_000)
        }
    };
    let trades: Vec<Value> = page
        .iter()
        .map(|((block, index), t)| {
            json!({
                "id": format!("{block}-{index}"),
                "time": t.ts,
                "type": match t.kind {
                    TradeKind::Swap if t.buy => "buy",
                    TradeKind::Swap => "sell",
                    TradeKind::Add => "add",
                    TradeKind::Remove => "remove",
                },
                "maker": hex(&t.maker),
                "tx": hex(&t.tx),
                "base_amount": num(t.base),
                "quote_amount": num(t.quote),
                "usd": num(t.usd as f64),
                "price_usd": num(t.price_usd as f64),
                "price_native": num(t.price_native as f64),
            })
        })
        .collect();
    let next = scanned.filter(|_| !page.is_empty() || q.maker.is_some()).map(|(b, i)| format!("{b}-{i}"));
    Ok(json!({ "trades": trades, "next": next }))
}

async fn trades(Ax(app): A, Path(id): Path<String>, Query(q): Query<TradesQ>) -> Response {
    let st = read(&app);
    let Some(p) = pool_of(&st, &id) else { return err(StatusCode::NOT_FOUND, "unknown pair or token") };
    match trade_json(&app.sh.store, p, &q) {
        Ok(v) => ok(v, if q.before.is_some() { 86_400 } else { 3 }),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn token(Ax(app): A, Path(addr): Path<String>) -> Response {
    let st = read(&app);
    let Some(t) =
        unhex(addr.trim()).and_then(|b| <[u8; 20]>::try_from(b).ok()).and_then(|a| st.token_ix.get(&a).copied())
    else {
        return err(StatusCode::NOT_FOUND, "unknown token");
    };
    let tok = &st.tokens[t as usize];
    let get = |ks: &fjall::Keyspace| -> Value {
        ks.get(tok.addr).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or(Value::Null)
    };
    let mut pools = st.base_pools.get(&t).cloned().unwrap_or_default();
    pools.sort_unstable_by(|a, b| st.liq(*b).total_cmp(&st.liq(*a)));
    let mut token = token_json(&st, t);
    token["supply"] = num(tok.supply);
    token["fdv_usd"] = num(tok.supply * tok.usd);
    token["best_pair"] = (tok.best_pool != NONE).then(|| st.pools[tok.best_pool as usize].key()).into();
    let profile = get(&app.sh.store.profiles);
    token["description"] = profile["d"].clone();
    token["links"] = profile["l"].clone();
    token["safety"] = get(&app.sh.store.safety);
    let pairs: Vec<Value> = pools.iter().take(50).map(|&p| pair_json(&app, &st, p)).collect();
    ok(json!({ "token": token, "pairs": pairs }), 10)
}

async fn list(Ax(app): A, Path(tab): Path<String>, Query(mut f): Query<Filter>) -> Response {
    if !TABS.iter().any(|t| t.1 == tab && t.0 != lists::Tab::Watch) {
        return err(StatusCode::NOT_FOUND, "tab is trending, top, gainers, losers or new");
    }
    f.tab = Some(tab);
    let pools = app.list(&f);
    let st = read(&app);
    let pairs: Vec<Value> = pools.iter().map(|&p| pair_json(&app, &st, p)).collect();
    ok(json!({ "pairs": pairs }), 5)
}
