//! JSON-RPC to our Base node: live logs, transaction senders and token metadata.

use crate::abi::{self, Addr, RawLog, T, V4_POOL_MANAGER, hex, unhex};
use crate::state::Meta;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

const MULTICALL3: &str = "0xca11bde05977b3631167028862be2a173976ca11";
const DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
const SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
const NAME: [u8; 4] = [0x06, 0xfd, 0xde, 0x03];
const TOTAL_SUPPLY: [u8; 4] = [0x18, 0x16, 0x0d, 0xdd];
/// A block without logs whose bloom allows our events is trusted this many blocks deep (~5 s).
const DEPTH: u32 = 25;
/// Polls (every 500 ms) for such a block's logs before failing, so the indexer retries.
const MAX_POLLS: u32 = if cfg!(test) { 4 } else { 40 };

/// A future that calls itself (splitting a batch), so it has to be boxed.
type Recursive<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Clone)]
pub struct Rpc {
    url: String,
    http: reqwest::Client,
}

impl Rpc {
    pub fn new(url: &str) -> Rpc {
        Rpc { url: url.into(), http: reqwest::Client::new() }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut out = self.batch(vec![(method, params)]).await?;
        Ok(out.pop().unwrap_or(Value::Null))
    }

    /// Sends several calls in one request; results come back in order.
    async fn batch(&self, calls: Vec<(&str, Value)>) -> Result<Vec<Value>> {
        // The node answers an empty batch with an error object, not an empty list.
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (m, p))| json!({"jsonrpc": "2.0", "id": i, "method": m, "params": p}))
            .collect();
        let resp: Vec<Value> = self.http.post(&self.url).json(&body).send().await?.error_for_status()?.json().await?;
        let mut out = vec![Value::Null; calls.len()];
        for r in resp {
            let id = r["id"].as_u64().unwrap_or(0) as usize;
            if let Some(e) = r.get("error") {
                return Err(RpcError(e.clone()).into());
            }
            if id < out.len() {
                out[id] = r["result"].clone();
            }
        }
        Ok(out)
    }

    pub async fn block_number(&self) -> Result<u32> {
        let v = self.call("eth_blockNumber", json!([])).await?;
        num(&v).context("eth_blockNumber")
    }

    /// Our logs in `[from, to]`, ordered, with block timestamps and transaction senders.
    pub async fn logs(&self, from: u32, to: u32) -> Result<Vec<RawLog>> {
        let headers = self.headers(from, to).await?;
        // The caller saves the range as done, so a block without logs must not just be one
        // whose receipts the node hasn't stored yet. Trust it when its bloom rules our events
        // out, per filter; a bloom also matches by chance, so trust it anyway once the node's
        // head is `DEPTH` blocks past it (by depth, so no clock can make a fresh block look old).
        let mut res = self.raw_logs(from, to).await?;
        for poll in 0.. {
            let have: HashSet<(u32, usize)> =
                res.iter().filter_map(|l| Some((num(&l["blockNumber"])?, filter_of(l)))).collect();
            let missing: Vec<u32> = (from..=to)
                .zip(&headers)
                .filter(|&(b, (_, bloom))| (0..2).any(|f| bloom[f] && !have.contains(&(b, f))))
                .map(|(b, _)| b)
                .collect();
            let Some(&newest) = missing.last() else { break };
            if self.block_number().await? >= newest.saturating_add(DEPTH) {
                if poll > 0 {
                    eprintln!("indexer: no logs in blocks {missing:?} although their blooms allow them");
                }
                break;
            }
            if poll == MAX_POLLS {
                bail!("blocks {missing:?}: no logs yet, although their blooms allow them");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            res = self.raw_logs(from, to).await?;
        }
        let mut logs: Vec<RawLog> = res.iter().filter_map(parse_log).collect();
        logs.sort_by_key(|l| (l.block, l.index));
        let mut hashes: Vec<[u8; 32]> = logs.iter().map(|l| l.tx).collect();
        hashes.sort();
        hashes.dedup();
        let mut senders = HashMap::new();
        for chunk in hashes.chunks(200) {
            let calls = chunk.iter().map(|h| ("eth_getTransactionByHash", json!([hex(h)]))).collect();
            for (h, tx) in chunk.iter().zip(self.batch(calls).await?) {
                senders.insert(*h, addr20(&tx["from"]).unwrap_or_default());
            }
        }
        for l in &mut logs {
            l.from = senders.get(&l.tx).copied().unwrap_or_default();
            l.ts = l
                .block
                .checked_sub(from)
                .and_then(|i| headers.get(i as usize))
                .map(|h| h.0)
                .context("block timestamp")?;
        }
        Ok(logs)
    }

    /// Timestamp of each block in `[from, to]`, and which of our filters its logs bloom allows.
    /// A block the node can't show is an error: its logs can't be vouched for.
    async fn headers(&self, from: u32, to: u32) -> Result<Vec<(u32, [bool; 2])>> {
        let blocks: Vec<u32> = (from..=to).collect();
        let mut out = Vec::with_capacity(blocks.len());
        for chunk in blocks.chunks(200) {
            let calls = chunk.iter().map(|b| ("eth_getBlockByNumber", json!([format!("0x{b:x}"), false]))).collect();
            for (b, h) in chunk.iter().zip(self.batch(calls).await?) {
                let ts = num(&h["timestamp"]).context(format!("block {b}: no timestamp"))?;
                let bloom = h["logsBloom"].as_str().and_then(unhex).filter(|b| b.len() == 256);
                out.push((ts, bloom_matches(&bloom.context(format!("block {b}: no logs bloom"))?)));
            }
        }
        Ok(out)
    }

    /// Every raw log of our filters in `[from, to]`. The node refuses a range with too many
    /// results ("query exceeds max results 20000, retry with the range A-B"), so on an error
    /// retry up to the end it suggests (or half the range) and grow the step back after.
    /// Never returns part of the range: the caller stores `to` as done.
    async fn raw_logs(&self, from: u32, to: u32) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let (mut at, mut step) = (from, to.saturating_sub(from) + 1);
        while at <= to {
            let end = to.min(at.saturating_add(step - 1));
            match self.get_logs(at, end).await {
                Ok(logs) => {
                    out.extend(logs);
                    step = step.saturating_mul(2);
                    at = end + 1;
                }
                Err(e) if too_many(&e) && end > at => {
                    // The hint must shrink the range, or a bad one could loop forever.
                    let cut = hint_end(&e).filter(|&b| b >= at && b < end).unwrap_or(at + (end - at) / 2);
                    step = cut - at + 1;
                }
                Err(e) => return Err(e.context(format!("eth_getLogs {at}..={end}"))),
            }
        }
        Ok(out)
    }

    /// One request with both getLogs filters over `[from, to]`.
    async fn get_logs(&self, from: u32, to: u32) -> Result<Vec<Value>> {
        let range = |f: Value| {
            let mut f = f;
            f["fromBlock"] = json!(format!("0x{from:x}"));
            f["toBlock"] = json!(format!("0x{to:x}"));
            f
        };
        let topics = |ts: Vec<[u8; 32]>| ts.iter().map(|t| hex(t)).collect::<Vec<_>>();
        let res = self
            .batch(vec![
                ("eth_getLogs", json!([range(json!({"topics": [topics(T.any_address())]}))])),
                ("eth_getLogs", json!([range(json!({"address": hex(&V4_POOL_MANAGER), "topics": [topics(T.v4())]}))])),
            ])
            .await?;
        // A missing or odd result would read as "no logs" and skip those blocks for good.
        let mut out = Vec::new();
        for r in res {
            let Value::Array(a) = r else { bail!("eth_getLogs {from}..={to}: no log list in the reply") };
            out.extend(a);
        }
        Ok(out)
    }

    /// Decimals, symbol, name and total supply of each token, via Multicall3.
    pub async fn meta(&self, tokens: &[Addr]) -> Result<Vec<Meta>> {
        let mut out = Vec::with_capacity(tokens.len());
        for chunk in tokens.chunks(150) {
            out.extend(self.meta_batch(chunk).await?);
        }
        Ok(out)
    }

    /// One multicall. When the node rejects the whole call (a token burning all the gas),
    /// split it until the culprit is alone. Network errors propagate so the batch retries:
    /// guessing decimals would corrupt every price of that token for good.
    fn meta_batch<'a>(&'a self, tokens: &'a [Addr]) -> Recursive<'a, Vec<Meta>> {
        Box::pin(async move {
            let calls: Vec<(Addr, [u8; 4])> =
                tokens.iter().flat_map(|t| [DECIMALS, SYMBOL, NAME, TOTAL_SUPPLY].map(|sel| (*t, sel))).collect();
            let data = hex(&encode_aggregate3(&calls));
            let ret = match self.call("eth_call", json!([{"to": MULTICALL3, "data": data}, "latest"])).await {
                Ok(v) => decode_aggregate3(&unhex(v.as_str().unwrap_or_default()).unwrap_or_default()),
                Err(e) if e.is::<RpcError>() && tokens.len() > 1 => {
                    let (a, b) = tokens.split_at(tokens.len() / 2);
                    let mut out = self.meta_batch(a).await?;
                    out.extend(self.meta_batch(b).await?);
                    return Ok(out);
                }
                Err(e) if e.is::<RpcError>() => vec![None; 4], // a lone broken token: unnamed, 18 decimals
                Err(e) => return Err(e),
            };
            if ret.len() != calls.len() {
                bail!("multicall returned {} results for {} calls", ret.len(), calls.len());
            }
            Ok((0..tokens.len())
                .map(|i| {
                    let get = |k: usize| ret[i * 4 + k].as_deref();
                    // Tokens without decimals() are treated as 18, like most wallets do.
                    let decimals = get(0).filter(|d| d.len() == 32).map(|d| d[31].min(36)).unwrap_or(18);
                    let scale = 10f64.powi(decimals as i32);
                    Meta {
                        decimals,
                        symbol: get(1).map(|d| text(d, 24)).unwrap_or_default(),
                        name: get(2).map(|d| text(d, 64)).unwrap_or_default(),
                        supply: get(3)
                            .and_then(|d| d.get(..32)?.try_into().ok())
                            .map(|w| abi::u256_f64(w) / scale)
                            .unwrap_or(0.0),
                    }
                })
                .collect())
        })
    }

    /// Whether each Solidly pool is on the stable curve, via Multicall3. A pool whose
    /// `stable()` reverts isn't a stable pool; one breaking the whole call is split out as in
    /// `meta_batch`, and left unknown (`None`).
    pub fn stable<'a>(&'a self, pools: &'a [Addr]) -> Recursive<'a, Vec<Option<bool>>> {
        Box::pin(async move {
            let sel = abi::keccak("stable()");
            let calls: Vec<(Addr, [u8; 4])> = pools.iter().map(|p| (*p, [sel[0], sel[1], sel[2], sel[3]])).collect();
            let data = hex(&encode_aggregate3(&calls));
            let ret = match self.call("eth_call", json!([{"to": MULTICALL3, "data": data}, "latest"])).await {
                Ok(v) => decode_aggregate3(&unhex(v.as_str().unwrap_or_default()).unwrap_or_default()),
                Err(e) if e.is::<RpcError>() && pools.len() > 1 => {
                    let (a, b) = pools.split_at(pools.len() / 2);
                    let mut out = self.stable(a).await?;
                    out.extend(self.stable(b).await?);
                    return Ok(out);
                }
                Err(e) if e.is::<RpcError>() => return Ok(vec![None]),
                Err(e) => return Err(e),
            };
            // A malformed answer (`null`, `0x`) would fail the same way on every retry: leave
            // these pools unknown (x·y) rather than hold the indexer up; the next start retries.
            if ret.len() != pools.len() {
                eprintln!("stable(): multicall returned {} results for {} calls", ret.len(), pools.len());
                return Ok(vec![None; pools.len()]);
            }
            Ok(ret.iter().map(|r| Some(r.as_deref().is_some_and(|d| d.len() == 32 && d[31] == 1))).collect())
        })
    }

    /// String getters (`sels`) of one token; `None` where the call reverts or is empty.
    pub async fn strings(&self, token: Addr, sels: &[[u8; 4]]) -> Result<Vec<Option<String>>> {
        let calls: Vec<(Addr, [u8; 4])> = sels.iter().map(|s| (token, *s)).collect();
        let data = hex(&encode_aggregate3(&calls));
        let ret = match self.call("eth_call", json!([{"to": MULTICALL3, "data": data}, "latest"])).await {
            Ok(v) => decode_aggregate3(&unhex(v.as_str().unwrap_or_default()).unwrap_or_default()),
            Err(e) if e.is::<RpcError>() => vec![None; sels.len()],
            Err(e) => return Err(e),
        };
        Ok(ret.iter().map(|r| r.as_deref().map(|d| text(d, 1 << 20)).filter(|s| !s.is_empty())).collect())
    }
}

/// An error the node returned for a call (as opposed to not reaching the node).
#[derive(Debug)]
pub struct RpcError(Value);

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error: {}", self.0)
    }
}

impl std::error::Error for RpcError {}

/// The node's message when it refuses a call.
fn rpc_message(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<RpcError>()?.0["message"].as_str()
}

/// Whether the node refused a getLogs call for its size ("query exceeds max results",
/// "block range too large", ...), so a smaller range can succeed. Other node errors
/// (pruned history, a block past the head) wouldn't, so they fail at once.
fn too_many(e: &anyhow::Error) -> bool {
    let msg = rpc_message(e).unwrap_or_default().to_lowercase();
    ["result", "too large", "exceed"].iter().any(|w| msg.contains(w))
}

/// The last block of the range a node suggests after refusing a getLogs call.
fn hint_end(e: &anyhow::Error) -> Option<u32> {
    rpc_message(e)?.rsplit_once('-')?.1.trim().parse().ok()
}

/// Which of our two `get_logs` filters a 2048-bit logs bloom may hold an event of: any log
/// with a topic of `T.any_address()`, and a v4 topic from the pool manager. False positives
/// happen, false negatives don't.
fn bloom_matches(bloom: &[u8]) -> [bool; 2] {
    let has = |item: &[u8]| bloom_has(bloom, item);
    [T.any_address().iter().any(|t| has(t)), has(&V4_POOL_MANAGER) && T.v4().iter().any(|t| has(t))]
}

/// Which `get_logs` filter returned a log: 1 for a v4 event of the pool manager, else 0.
fn filter_of(l: &Value) -> usize {
    let topic = l["topics"][0].as_str().and_then(unhex);
    usize::from(
        addr20(&l["address"]) == Some(V4_POOL_MANAGER) && topic.is_some_and(|t| T.v4().iter().any(|v| v[..] == t[..])),
    )
}

/// Whether the bloom has the three bits of `item` (an address or a topic) set.
fn bloom_has(bloom: &[u8], item: &[u8]) -> bool {
    let h = abi::keccak(item);
    (0..3).all(|i| {
        let bit = (usize::from(h[2 * i]) << 8 | usize::from(h[2 * i + 1])) & 2047;
        bloom.get(255 - bit / 8).is_some_and(|byte| byte & 1 << (bit % 8) != 0)
    })
}

fn num(v: &Value) -> Option<u32> {
    u32::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

fn addr20(v: &Value) -> Option<Addr> {
    unhex(v.as_str()?)?.try_into().ok()
}

fn parse_log(l: &Value) -> Option<RawLog> {
    Some(RawLog {
        block: num(&l["blockNumber"])?,
        ts: 0,
        index: num(&l["logIndex"])?,
        address: addr20(&l["address"])?,
        topics: l["topics"].as_array()?.iter().filter_map(|t| unhex(t.as_str()?)?.try_into().ok()).collect(),
        data: unhex(l["data"].as_str()?)?,
        tx: unhex(l["transactionHash"].as_str()?)?.try_into().ok()?,
        from: [0; 20],
    })
}

/// ABI-encodes `aggregate3((address,bool,bytes)[])` with allowFailure = true.
fn encode_aggregate3(calls: &[(Addr, [u8; 4])]) -> Vec<u8> {
    let word = |n: usize| {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&(n as u64).to_be_bytes());
        w
    };
    let mut out = vec![0x82, 0xad, 0x56, 0xcb];
    out.extend(word(32));
    out.extend(word(calls.len()));
    // Each element: address, bool, offset(0x60), length(4), 4 bytes padded = 5 words.
    for i in 0..calls.len() {
        out.extend(word(calls.len() * 32 + i * 160));
    }
    for (target, sel) in calls {
        out.extend(abi::pad(target));
        out.extend(word(1));
        out.extend(word(96));
        out.extend(word(4));
        let mut w = [0u8; 32];
        w[..4].copy_from_slice(sel);
        out.extend(w);
    }
    out
}

/// Decodes `(bool success, bytes data)[]`; failed calls become `None`.
fn decode_aggregate3(b: &[u8]) -> Vec<Option<Vec<u8>>> {
    let word = |at: usize| -> Option<usize> {
        let w = b.get(at..at + 32)?;
        if w[..24].iter().any(|&x| x != 0) {
            return None;
        }
        Some(u64::from_be_bytes(w[24..].try_into().ok()?) as usize)
    };
    let Some(arr) = word(0) else {
        return Vec::new();
    };
    let n = word(arr).unwrap_or(0).min(b.len() / 32);
    let base = arr + 32;
    (0..n)
        .map(|i| {
            let el = base + word(base + i * 32)?;
            if word(el)? != 1 {
                return None;
            }
            let data = el + word(el + 32)?;
            let len = word(data)?;
            b.get(data + 32..data + 32 + len).map(<[u8]>::to_vec)
        })
        .collect()
}

/// A token string from either an ABI `string` or a `bytes32`, cleaned for display.
fn text(d: &[u8], max: usize) -> String {
    let raw = if d.len() >= 64 {
        let len = u64::from_be_bytes(d[56..64].try_into().unwrap_or_default()) as usize;
        d.get(64..64 + len).unwrap_or_default()
    } else {
        d.split(|&c| c == 0).next().unwrap_or_default()
    };
    String::from_utf8_lossy(raw).chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn aggregate3_round_trip_shape() {
        let enc = encode_aggregate3(&[([1; 20], DECIMALS), ([2; 20], SYMBOL)]);
        assert_eq!(enc.len(), 4 + 32 * 2 + 32 * 2 + 160 * 2);
        // Response: [(true, abi string "HI"), (false, "")]
        let mut r = Vec::new();
        let w = |n: usize| {
            let mut x = [0u8; 32];
            x[24..].copy_from_slice(&(n as u64).to_be_bytes());
            x
        };
        r.extend(w(32)); // array offset
        r.extend(w(2)); // length
        r.extend(w(64)); // element 0 offset
        r.extend(w(64 + 32 * 6)); // element 1 offset
        // element 0: success, offset 64, len 96 (abi-encoded string)
        r.extend(w(1));
        r.extend(w(64));
        r.extend(w(96));
        r.extend(w(32));
        r.extend(w(2));
        let mut s = [0u8; 32];
        s[..2].copy_from_slice(b"HI");
        r.extend(s);
        // element 1: failure
        r.extend(w(0));
        r.extend(w(64));
        r.extend(w(0));
        let d = decode_aggregate3(&r);
        assert_eq!(d.len(), 2);
        assert_eq!(text(d[0].as_ref().unwrap(), 10), "HI");
        assert!(d[1].is_none());
        assert_eq!(text(b"WETH\0\0\0\0", 10), "WETH");
    }

    /// A fake node answering each call with `reply(call, n)` (`n`: requests before this one)
    /// and an empty batch with an error object, like reth. Also returns the request count.
    async fn fake<F>(reply: F) -> (Rpc, Arc<AtomicUsize>)
    where
        F: Fn(&Value, usize) -> Value + Clone + Send + Sync + 'static,
    {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(reqs): axum::Json<Vec<Value>>| async move {
                let n = n.fetch_add(1, Ordering::Relaxed);
                if reqs.is_empty() {
                    return axum::Json(
                        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Invalid request"}}),
                    );
                }
                let out = reqs.iter().map(|r| {
                    let mut o = reply(r, n);
                    o["jsonrpc"] = json!("2.0");
                    o["id"] = r["id"].clone();
                    o
                });
                axum::Json(Value::Array(out.collect()))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (Rpc::new(&url), calls)
    }

    /// A log at `block` as the getLogs filter `f` (`f["address"]` set: the v4 one) returns it.
    fn log_at(block: u32, f: &Value) -> Value {
        let (address, topic) = if f["address"].is_null() { ([1; 20], T.v2_sync) } else { (V4_POOL_MANAGER, T.v4_swap) };
        json!({"blockNumber": format!("0x{block:x}"), "logIndex": "0x0", "address": hex(&address),
            "topics": [hex(&topic)], "data": "0x", "transactionHash": hex(&[2; 32])})
    }

    /// A fake node: one log per block for each filter, refusing ranges over `limit`
    /// blocks the way reth does, with a hint built by `hint(from, limit)`.
    async fn fake_node(limit: u32, hint: fn(u32, u32) -> String) -> (Rpc, Arc<AtomicUsize>) {
        fake(move |r, _| {
            let f = &r["params"][0];
            let (a, b) = (num(&f["fromBlock"]).unwrap(), num(&f["toBlock"]).unwrap());
            if b - a + 1 > limit {
                let message = format!("query exceeds max results 20000, {}", hint(a, limit));
                return json!({"error": {"code": -32602, "message": message}});
            }
            json!({"result": (a..=b).map(|blk| log_at(blk, f)).collect::<Vec<_>>()})
        })
        .await
    }

    /// A fake node that, for the first `lag_for` requests, has no logs for the last block of
    /// a range from the filters in `lag`, then one log per block and filter. `bloom(b)` gives
    /// block `b`'s bloom byte (`None`: the node doesn't have it); `head(n)` its head.
    async fn lagging_node(
        lag_for: usize,
        lag: [bool; 2],
        bloom: fn(u32) -> Option<&'static str>,
        head: fn(usize) -> u32,
    ) -> (Rpc, Arc<AtomicUsize>) {
        fake(move |r, n| {
            let p = &r["params"][0];
            match r["method"].as_str().unwrap() {
                "eth_getLogs" => {
                    let (a, b) = (num(&p["fromBlock"]).unwrap(), num(&p["toBlock"]).unwrap());
                    let b =
                        if n < lag_for && lag[usize::from(!p["address"].is_null())] { b.wrapping_sub(1) } else { b };
                    json!({"result": (a..=b).map(|blk| log_at(blk, p)).collect::<Vec<_>>()})
                }
                "eth_getBlockByNumber" => match bloom(num(p).unwrap()) {
                    Some(x) => json!({"result": {"timestamp": "0x10", "logsBloom": format!("0x{}", x.repeat(256))}}),
                    None => json!({"result": null}),
                },
                "eth_blockNumber" => json!({"result": format!("0x{:x}", head(n))}),
                "eth_getTransactionByHash" => json!({"result": {"from": hex(&[3; 20])}}),
                m => panic!("unexpected {m}"),
            }
        })
        .await
    }

    #[tokio::test]
    async fn blocks_without_logs_are_trusted_only_when_bloom_or_depth_allow() {
        let blocks = |logs: &[RawLog]| logs.iter().map(|l| l.block).collect::<Vec<_>>();
        let (both, any, v4) = ([true, true], [true, false], [false, true]);
        // An empty batch is no request at all (the node would answer it with an error).
        let (rpc, calls) = lagging_node(0, both, |_| None, |_| 0).await;
        assert!(rpc.batch(vec![]).await.unwrap().is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        // Nothing in the bloom: an empty block is real, no retry.
        let (rpc, calls) = lagging_node(usize::MAX, both, |_| Some("00"), |_| 5).await;
        assert!(rpc.logs(5, 5).await.unwrap().is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 2, "one header batch, one getLogs");

        // The newest block's bloom allows our events but it has none yet: wait for them, even
        // though the rest of the range came back, and even when only one filter lags.
        for lag in [both, any, v4] {
            let (rpc, _) = lagging_node(4, lag, |b| Some(if b == 6 { "ff" } else { "00" }), |_| 6).await;
            let logs = rpc.logs(5, 6).await.unwrap();
            assert_eq!(blocks(&logs), [5, 5, 6, 6], "lag {lag:?}");
            assert!(logs.iter().all(|l| l.ts == 16 && l.from == [3; 20]));
        }

        // A lagging block whose bloom rules us out doesn't hold the range up.
        let (rpc, calls) = lagging_node(usize::MAX, both, |b| Some(if b == 6 { "00" } else { "ff" }), |_| 6).await;
        assert_eq!(blocks(&rpc.logs(5, 6).await.unwrap()), [5, 5]);
        assert_eq!(calls.load(Ordering::Relaxed), 3, "headers, getLogs, senders");

        // Still none once the node is `DEPTH` blocks past it: a chance match, trusted.
        let (rpc, calls) = lagging_node(usize::MAX, both, |_| Some("ff"), |n| 5 + 10 * n as u32).await;
        assert!(rpc.logs(5, 5).await.unwrap().is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 5, "headers, getLogs, head 25, getLogs, head 45");
        let (rpc, calls) = lagging_node(usize::MAX, both, |_| Some("ff"), |_| 5 + DEPTH).await;
        assert!(rpc.logs(5, 5).await.unwrap().is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        // A stalled node doesn't hold the indexer forever, nor gets the block saved.
        let (rpc, _) = lagging_node(usize::MAX, both, |_| Some("ff"), |_| 5).await;
        assert!(rpc.logs(5, 5).await.unwrap_err().to_string().contains("no logs yet"));

        // A block the node can't show can't be vouched for.
        let (rpc, _) = lagging_node(usize::MAX, both, |b| (b == 5).then_some("00"), |_| 99).await;
        assert!(rpc.logs(5, 6).await.is_err());
    }

    #[tokio::test]
    async fn malformed_stable_answer_leaves_pools_unknown() {
        for result in [json!(null), json!("0x")] {
            let (rpc, _) = fake(move |_, _| json!({"result": result.clone()})).await;
            assert_eq!(rpc.stable(&[[1; 20], [2; 20]]).await.unwrap(), [None, None]);
        }
    }

    #[test]
    fn blooms_of_real_blocks() {
        let bloom = |s: &str| unhex(s).unwrap();
        // Base block 61794474: v2 Sync and Swap logs among 12.
        let b = bloom(
            "0x00200000000020100800000884001000000000000000000000340000000000000100040001000000004000100002100000100a00000000000002000008600000048000000040000080000008000000200020000000081400001000080800000000c00020000400000000000000000000000000000000000000000010000040000000000800000000000000004000000000000000002000088000004000040010020000040000000000000000000080000000000000000000010000004000004000000102000100000000000000000000000000000000001000000600000000200010000020000000000000000000000400000000200820000000810000000008",
        );
        assert!(bloom_has(&b, &T.v2_sync) && bloom_has(&b, &T.v2_swap) && bloom_matches(&b)[0]);
        // Base block 61797382: a v4 Swap from the pool manager, no other event of ours.
        let b = bloom(
            "0x00000000000000000000000000001000001200200084000000000000000000000020812000000000000080000000100000808000040000000400000210080000410004000000000008000408000000000002000000080020000000000000008000800000020800200000000402004804040000800000000000000032008000040010000004004000208000000000000000000000000000004000000401044000004000000000000000000008000000000004004200008000000000000000400004020402000000200000c00000000000000800080000000200280002000020000000000000000000004000000000000000000400000000000000000000021040",
        );
        assert!(bloom_has(&b, &V4_POOL_MANAGER) && bloom_has(&b, &T.v4_swap));
        assert_eq!(bloom_matches(&b), [false, true]);
        assert_eq!(bloom_matches(&[0; 256]), [false, false]);
    }

    #[tokio::test]
    async fn get_logs_splits_dense_ranges() {
        let hints: [fn(u32, u32) -> String; 4] = [
            |a, l| format!("retry with the range {a}-{}", a + l - 1), // usable
            |a, l| format!("retry with the range {a}-{}", a + l),     // still one too many
            |a, _| format!("retry with the range {a}-{}", a.saturating_sub(5)), // before the start
            |_, _| String::new(),                                     // no hint
        ];
        for (i, hint) in hints.into_iter().enumerate() {
            let (rpc, calls) = fake_node(37, hint).await;
            let logs = rpc.raw_logs(1_000, 1_499).await.unwrap();
            let blocks: Vec<u32> = logs.iter().filter_map(|l| num(&l["blockNumber"])).collect();
            // Each block once from each filter, nothing skipped or repeated.
            let want: Vec<u32> = (1_000..1_500).flat_map(|b| [b, b]).collect();
            let mut got = blocks.clone();
            got.sort();
            assert_eq!(got, want, "hint {i}");
            assert!(calls.load(Ordering::Relaxed) < 100, "hint {i}: {} calls", calls.load(Ordering::Relaxed));
        }
        // A single block over the limit can't be split: an error, not a hang.
        let (rpc, _) = fake_node(0, |_, _| String::new()).await;
        assert!(rpc.raw_logs(5, 9).await.is_err());

        let refusal = |m: &str| anyhow::Error::new(RpcError(json!({"code": -32602, "message": m})));
        let reth = refusal("query exceeds max results 20000, retry with the range 61258906-61259315");
        assert!(too_many(&reth));
        assert_eq!(hint_end(&reth), Some(61259315));
        assert!(!too_many(&refusal("pruned history unavailable: requested 0, earliest available 60000000")));
        assert!(!too_many(&anyhow::anyhow!("connection refused")));
    }
}
