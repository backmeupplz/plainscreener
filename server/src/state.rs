//! In-memory index state: every token and pool, open candles and 24 h stats.
//! The indexer applies logs in chain order and flushes changes to the store in batches.

use crate::abi::{self, Addr, Event, Id, Kind, RawLog, WETH, hex20};
use crate::model::{Candle, Pool, RES, Stats, Token, Trade};
use crate::store::{Store, candle_key, trade_key};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

pub const NONE: u32 = u32::MAX;
/// Quote-side liquidity a pool needs before its trades may set a token's USD price.
const MIN_REF_LIQ: f64 = 1_000.0;
const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;

const STABLES: [Addr; 4] = [
    hex20("833589fcd6edb6e08f4c7c32d4f71b54bda02913"), // USDC
    hex20("d9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca"), // USDbC
    hex20("fde4c96c8593536e31f229ea8f37b2ada2699bb2"), // USDT
    hex20("50c5725949a6f0c72e6c4a641f24049a917db0cb"), // DAI
];

/// Token metadata read from chain.
pub struct Meta {
    pub decimals: u8,
    pub symbol: String,
    pub name: String,
    pub supply: f64,
}

#[derive(Default)]
pub struct State {
    pub tokens: Vec<Token>,
    pub token_ix: HashMap<Addr, u32>,
    pub pools: Vec<Pool>,
    pub pool_ix: HashMap<Id, u32>,
    /// Pools from known DEXes by their base token, for a token's other pairs.
    pub base_pools: HashMap<u32, Vec<u32>>,
    pub stats: HashMap<u32, Box<Stats>>,
    /// Timestamp of the last applied block.
    pub head_ts: u32,
    /// Open candle per (pool, resolution index).
    candles: HashMap<(u32, usize), (u32, Candle)>,
    dirty_pools: HashSet<u32>,
    dirty_tokens: HashSet<u32>,
    dirty_candles: HashSet<(u32, usize)>,
    /// Candles that closed since the last flush (a batch spans many candle periods).
    closed_candles: Vec<([u8; 9], Vec<u8>)>,
    trades: Vec<([u8; 12], Vec<u8>)>,
    /// Pools that traded since the last flush (for live updates).
    touched: HashSet<u32>,
    /// Hour of the last pruning of idle pools' stats.
    pruned_hour: u32,
}

impl State {
    pub fn load(store: &Store) -> Result<State> {
        let mut s = State { tokens: store.load_tokens()?, pools: store.load_pools()?, ..Default::default() };
        s.token_ix = s.tokens.iter().enumerate().map(|(i, t)| (t.addr, i as u32)).collect();
        for g in store.icons.range::<&[u8], _>(..) {
            let (k, v) = g.into_inner()?;
            if v.len() > 4
                && let Some(&t) = <[u8; 20]>::try_from(&*k).ok().and_then(|a| s.token_ix.get(&a))
            {
                s.tokens[t as usize].icon = true;
            }
        }
        s.pool_ix = s.pools.iter().enumerate().map(|(i, p)| (p.id, i as u32)).collect();
        for (i, p) in s.pools.iter().enumerate().filter(|(_, p)| p.trusted()) {
            s.base_pools.entry(p.base()).or_default().push(i as u32);
        }
        s.head_ts = store.cursor()?.map(|(_, ts)| ts).unwrap_or(0);
        // Rebuild 24 h stats from stored 1 m and 1 h candles.
        let now = s.head_ts;
        for (i, _) in s.pools.iter().enumerate().filter(|(_, p)| p.last_ts + 86_400 > now) {
            let st = s.stats.entry(i as u32).or_insert_with(Stats::new);
            for (ri, span) in [(0, 3600), (3, 86_400)] {
                for (t, c) in store.candles(i as u32, ri, now.saturating_sub(span), now + 1, 60) {
                    st.restore(RES[ri], t, &c);
                }
            }
        }
        Ok(s)
    }

    /// Tokens referenced by pool creations in `logs` that we have never seen, with the
    /// block they first appear in, in order.
    pub fn unknown_tokens(&self, logs: &[RawLog]) -> Vec<(Addr, u32)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for log in logs {
            if let Some((_, Event::Created { token0, token1, .. })) = abi::decode(log) {
                for t in [token0, token1] {
                    if !self.token_ix.contains_key(&t) && seen.insert(t) {
                        out.push((t, log.block));
                    }
                }
            }
        }
        out
    }

    pub fn add_token(&mut self, addr: Addr, meta: Meta, block: u32) {
        let idx = self.tokens.len() as u32;
        let stable = STABLES.contains(&addr);
        self.tokens.push(Token {
            addr,
            decimals: meta.decimals,
            symbol: meta.symbol,
            name: meta.name,
            supply: meta.supply,
            usd: if stable { 1.0 } else { f64::NAN },
            ref_pool: NONE,
            best_pool: NONE,
            first_block: block,
            icon: false,
        });
        self.token_ix.insert(addr, idx);
        self.dirty_tokens.insert(idx);
    }

    pub fn set_supply(&mut self, token: u32, supply: f64) {
        self.tokens[token as usize].supply = supply;
        self.dirty_tokens.insert(token);
    }

    fn scale(&self, token: u32) -> f64 {
        10f64.powi(self.tokens[token as usize].decimals as i32)
    }

    /// Lower ranks are preferred as the quote side of a pair.
    fn quote_rank(&self, token: u32) -> (u8, u32) {
        let addr = self.tokens[token as usize].addr;
        let class = if STABLES.contains(&addr) {
            0
        } else if addr == WETH {
            1
        } else {
            2
        };
        // Among the rest, the token we saw first is the more established one.
        (class, token)
    }

    /// USD value of a pool's quote-side balance (0 when unknown).
    fn quote_liq(&self, p: u32) -> f64 {
        let Some(pool) = self.pools.get(p as usize) else {
            return 0.0;
        };
        let v = pool.bal_quote() * self.tokens[pool.quote() as usize].usd;
        if v.is_finite() { v } else { 0.0 }
    }

    /// Pool liquidity in USD: twice the quote side. Valuing the base side at the pool's own
    /// price would let one trade at an absurd price inflate it without limit.
    pub fn liq(&self, p: u32) -> f64 {
        2.0 * self.quote_liq(p)
    }

    pub fn apply(&mut self, log: &RawLog, store: &Store) {
        let Some((id, ev)) = abi::decode(log) else {
            return;
        };
        let ts = log.ts;
        self.head_ts = self.head_ts.max(ts);
        if let Event::Created { kind, token0, token1, fee, sqrt_price, origin, .. } = ev {
            return self.create(id, kind, token0, token1, fee, sqrt_price, origin, log.block, ts);
        }
        let Some(&p) = self.pool_ix.get(&id) else {
            return;
        };
        let (s0, s1) = {
            let pool = &self.pools[p as usize];
            (self.scale(pool.token0), self.scale(pool.token1))
        };
        let pool = &mut self.pools[p as usize];
        match ev {
            Event::Sync { r0, r1 } => {
                pool.bal0 = r0 / s0;
                pool.bal1 = r1 / s1;
            }
            Event::SwapV2 { d0, d1 } => self.trade(p, log, ts, d0 / s0, d1 / s1, store),
            Event::SwapCl { d0, d1, sqrt_price, fee } => {
                let (d0, d1) = (d0 / s0, d1 / s1);
                // LP fees stay in the pool until withdrawn; v4 withdrawals don't report
                // them, so leave them out of v4 balances. v3 Collect includes them.
                let keep = if pool.kind == Kind::V4 { 1.0 - fee as f64 / 1e6 } else { 1.0 };
                pool.bal0 += if d0 > 0.0 { d0 * keep } else { d0 };
                pool.bal1 += if d1 > 0.0 { d1 * keep } else { d1 };
                pool.sqrt_price = sqrt_price;
                self.trade(p, log, ts, d0, d1, store)
            }
            Event::Flow { d0, d1 } => {
                pool.bal0 += d0 / s0;
                pool.bal1 += d1 / s1;
            }
            Event::Modify { tick_lower, tick_upper, liquidity } => {
                let (a0, a1) = amounts(pool.sqrt_price / Q96, tick_lower, tick_upper, liquidity);
                pool.bal0 += a0 / s0;
                pool.bal1 += a1 / s1;
            }
            Event::Created { .. } => {}
        }
        self.dirty_pools.insert(p);
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        id: Id,
        kind: Kind,
        t0: Addr,
        t1: Addr,
        fee: u32,
        sqrt: f64,
        origin: Addr,
        block: u32,
        ts: u32,
    ) {
        let (Some(&token0), Some(&token1)) = (self.token_ix.get(&t0), self.token_ix.get(&t1)) else {
            return;
        };
        if self.pool_ix.contains_key(&id) || token0 == token1 {
            return;
        }
        let quote0 = self.quote_rank(token0) < self.quote_rank(token1);
        let mut pool = Pool {
            id,
            kind,
            origin,
            token0,
            token1,
            quote0,
            fee,
            created_block: block,
            created_ts: ts,
            bal0: 0.0,
            bal1: 0.0,
            sqrt_price: sqrt,
            price_native: f64::NAN,
            price_usd: f64::NAN,
            last_ts: ts,
        };
        if sqrt > 0.0 {
            let p1per0 = (sqrt / Q96).powi(2) * self.scale(token0) / self.scale(token1);
            pool.price_native = if quote0 { 1.0 / p1per0 } else { p1per0 };
        }
        let p = self.pools.len() as u32;
        if pool.trusted() {
            self.base_pools.entry(pool.base()).or_default().push(p);
        }
        self.pools.push(pool);
        self.pool_ix.insert(id, p);
        self.dirty_pools.insert(p);
    }

    /// Records a swap given the pool's balance deltas in whole tokens. The price is what
    /// was actually exchanged: a concentrated-liquidity pool's spot price after a swap
    /// through an empty range sits at the protocol limit, which nobody traded at.
    fn trade(&mut self, p: u32, log: &RawLog, ts: u32, d0: f64, d1: f64, store: &Store) {
        let pool = &self.pools[p as usize];
        let (db, dq) = if pool.quote0 { (d1, d0) } else { (d0, d1) };
        if db == 0.0 || dq == 0.0 {
            return;
        }
        let (base, quote) = (pool.base(), pool.quote());
        let native = dq.abs() / db.abs();
        if !native.is_finite() || native <= 0.0 {
            return;
        }
        let qusd = self.tokens[quote as usize].usd;
        let usd = native * qusd;
        let vol = dq.abs() * qusd;
        let buy = db < 0.0;
        let (prev_usd, prev_native) = (pool.price_usd, pool.price_native);

        for (ri, res) in RES.iter().enumerate() {
            let t = ts - ts % res;
            let open = match self.candles.get(&(p, ri)) {
                Some((ct, _)) if *ct == t => None,
                previous => {
                    // The previous candle closes; keep it for the flush before replacing it.
                    if let Some((pt, pc)) = previous
                        && self.dirty_candles.contains(&(p, ri))
                    {
                        self.closed_candles.push((candle_key(p, ri, *pt), pc.encode()));
                    }
                    Some(store.candles.get(candle_key(p, ri, t)).ok().flatten())
                }
            };
            let entry = self.candles.entry((p, ri)).or_insert((t, Candle::default()));
            if let Some(stored) = open {
                let o = |prev: f64, cur: f64| {
                    if prev.is_finite() && prev > 0.0 { prev } else { cur }
                };
                let mut c = Candle::open(o(prev_usd, usd), o(prev_native, native));
                if let Some(b) = stored {
                    c = Candle::decode(&b);
                }
                *entry = (t, c);
            }
            entry.1.add(usd, native, vol, buy);
            self.dirty_candles.insert((p, ri));
        }
        let open = if prev_usd.is_finite() { prev_usd } else { usd };
        self.stats.entry(p).or_insert_with(Stats::new).add(ts, open, vol, buy);
        let trade = Trade {
            ts,
            tx: log.tx,
            maker: log.from,
            buy,
            base: db.abs(),
            quote: dq.abs(),
            usd: vol as f32,
            price_usd: usd as f32,
            price_native: native as f32,
        };
        self.trades.push((trade_key(p, log.block, log.index), trade.encode()));

        let pool = &mut self.pools[p as usize];
        pool.price_native = native;
        pool.price_usd = usd;
        pool.last_ts = ts;
        let trusted = pool.trusted();
        // The deepest pool sets the price, judged by liquidity now (a drained or rugged
        // reference pool loses its claim at once).
        let qliq = self.quote_liq(p);
        let tok = &self.tokens[base as usize];
        if trusted
            && !STABLES.contains(&tok.addr)
            && usd.is_finite()
            && qliq >= MIN_REF_LIQ
            && (tok.ref_pool == p || !tok.usd.is_finite() || qliq > self.quote_liq(tok.ref_pool))
        {
            let tok = &mut self.tokens[base as usize];
            tok.usd = usd;
            tok.ref_pool = p;
            self.dirty_tokens.insert(base);
        }
        // A token links to its most liquid trusted pool, or any pool if it has no trusted one.
        let rank = |s: &State, q: u32| (s.pools[q as usize].trusted(), s.liq(q));
        for t in [base, quote] {
            let best = self.tokens[t as usize].best_pool;
            if best != p && (best == NONE || rank(self, p) > rank(self, best)) {
                self.tokens[t as usize].best_pool = p;
                self.dirty_tokens.insert(t);
            }
        }
        self.touched.insert(p);
    }

    /// Writes everything changed since the last flush, with the next block to index.
    /// Returns the pools that traded.
    pub fn flush(&mut self, store: &Store, cursor: u32) -> Result<Vec<u32>> {
        let mut b = store.db.batch();
        for p in self.dirty_pools.drain() {
            b.insert(&store.pools, p.to_be_bytes().to_vec(), self.pools[p as usize].encode());
        }
        for t in self.dirty_tokens.drain() {
            b.insert(&store.tokens, t.to_be_bytes().to_vec(), self.tokens[t as usize].encode());
        }
        for (k, v) in self.closed_candles.drain(..) {
            b.insert(&store.candles, k.to_vec(), v);
        }
        for (p, ri) in self.dirty_candles.drain() {
            if let Some((t, c)) = self.candles.get(&(p, ri)) {
                b.insert(&store.candles, candle_key(p, ri, *t).to_vec(), c.encode());
            }
        }
        for (k, v) in self.trades.drain(..) {
            b.insert(&store.trades, k.to_vec(), v);
        }
        store.set_cursor(&mut b, cursor, self.head_ts);
        b.commit()?;
        // Candles whose period has ended can't change any more.
        let head = self.head_ts;
        self.candles.retain(|&(_, ri), (t, _)| *t + RES[ri] > head);
        if head / 3600 != self.pruned_hour {
            self.pruned_hour = head / 3600;
            self.stats.retain(|_, s| s.last_active() + 86_400 > head);
        }
        Ok(self.touched.drain().collect())
    }

    pub fn pending(&self) -> usize {
        self.trades.len() + self.dirty_candles.len() + self.dirty_pools.len()
    }
}

/// Token amounts (raw units) for a v4 liquidity change at the current price.
fn amounts(sqrt_p: f64, tick_lower: i32, tick_upper: i32, liquidity: f64) -> (f64, f64) {
    let sa = 1.0001f64.powf(tick_lower as f64 / 2.0);
    let sb = 1.0001f64.powf(tick_upper as f64 / 2.0);
    if sqrt_p <= sa {
        (liquidity * (sb - sa) / (sa * sb), 0.0)
    } else if sqrt_p < sb {
        (liquidity * (sb - sqrt_p) / (sqrt_p * sb), liquidity * (sqrt_p - sa))
    } else {
        (0.0, liquidity * (sb - sa))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{T, pad};

    fn word(x: f64) -> Vec<u8> {
        let mut w = [0u8; 32];
        let mut v = x;
        for i in (0..32).rev() {
            w[i] = (v % 256.0) as u8;
            v = (v / 256.0).floor();
        }
        w.to_vec()
    }

    fn log(block: u32, address: Addr, topics: Vec<[u8; 32]>, data: Vec<u8>) -> RawLog {
        RawLog { block, ts: 1_794_992_700 + block, address, topics, data, ..Default::default() }
    }

    fn meta(symbol: &str, decimals: u8) -> Meta {
        Meta { decimals, symbol: symbol.into(), name: symbol.into(), supply: 1e9 }
    }

    #[test]
    fn v2_pool_prices_weth_then_a_token() {
        let dir = std::env::temp_dir().join(format!("ps-test-{}", std::process::id()));
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let usdc = STABLES[0];
        let meme = [9u8; 20];
        s.add_token(WETH, meta("WETH", 18), 1);
        s.add_token(usdc, meta("USDC", 6), 1);
        s.add_token(meme, meta("MEME", 18), 1);
        let uniswap = hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6");
        let (pool_a, pool_b, fake) = ([2u8; 20], [3u8; 20], [4u8; 20]);
        let created_by = |factory: Addr, pool: Addr, t0: Addr, t1: Addr| {
            let mut data = pad(&pool).to_vec();
            data.extend(word(1.0));
            log(10, factory, vec![T.v2_pair_created, pad(&t0), pad(&t1)], data)
        };
        let created = |pool: Addr, t0: Addr, t1: Addr| created_by(uniswap, pool, t0, t1);
        // WETH/USDC with $3M on each side: 1000 WETH @ $3000.
        s.apply(&created(pool_a, WETH, usdc), &store);
        let sync = |pool: Addr, r0: f64, r1: f64| log(11, pool, vec![T.v2_sync], [word(r0), word(r1)].concat());
        let swap = |pool: Addr, a0in: f64, a1in: f64, a0out: f64, a1out: f64| {
            let data = [word(a0in), word(a1in), word(a0out), word(a1out)].concat();
            log(11, pool, vec![T.v2_swap, [0; 32], [0; 32]], data)
        };
        s.apply(&sync(pool_a, 1000e18, 3_000_000e6), &store);
        s.apply(&swap(pool_a, 0.0, 3000e6, 1e18, 0.0), &store); // buy 1 WETH for 3000 USDC
        let weth = &s.tokens[0];
        assert!((weth.usd - 3000.0).abs() < 1e-6, "WETH priced from USDC: {}", weth.usd);
        assert!(!s.pools[0].quote0, "USDC (token1) is the quote");

        // A contract posing as a bigger WETH/USDC pool can't move WETH's price.
        s.apply(&created_by([1; 20], fake, WETH, usdc), &store);
        s.apply(&sync(fake, 1e6 * 1e18, 1e12 * 1e6), &store);
        s.apply(&swap(fake, 0.0, 1e6, 1e15, 0.0), &store); // "WETH at $1000"
        assert!((s.tokens[0].usd - 3000.0).abs() < 1e-6, "untrusted pool ignored: {}", s.tokens[0].usd);
        assert_eq!(s.tokens[0].best_pool, 0, "WETH still links to the real pool");

        // MEME/WETH: 1M MEME vs 10 WETH ($30k). Sell 1000 MEME for 0.01 WETH.
        s.apply(&created(pool_b, meme, WETH), &store);
        s.apply(&sync(pool_b, 1_000_000e18, 10e18), &store);
        s.apply(&swap(pool_b, 1000e18, 0.0, 0.0, 0.01e18), &store);
        let m = &s.tokens[2];
        assert!((m.usd - 0.03).abs() < 1e-9, "MEME = 0.00001 WETH = $0.03, got {}", m.usd);
        assert!((s.liq(2) - 60_000.0).abs() < 1.0);
        assert_eq!(s.flush(&store, 12).unwrap().len(), 3);
        let candles = store.candles(2, 0, 0, u32::MAX, 10);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].1.sells, 1);
        assert_eq!(store.trades(2, (u32::MAX, 0), 10).len(), 1);

        // Trades in three different minutes before one flush keep three 1 m candles.
        for minute in 1..=3 {
            let mut l = swap(pool_b, 1000e18, 0.0, 0.0, 0.01e18);
            l.ts += minute * 60;
            s.apply(&l, &store);
        }
        s.flush(&store, 13).unwrap();
        assert_eq!(store.candles(2, 0, 0, u32::MAX, 10).len(), 4, "every closed candle is stored");

        // A smaller pool can't move the price while the deep one holds liquidity…
        let pool_c = [5u8; 20];
        s.apply(&created(pool_c, meme, WETH), &store);
        s.apply(&sync(pool_c, 50_000e18, 1e18), &store); // 1 WETH ($3k) of quote liquidity
        s.apply(&swap(pool_c, 10e18, 0.0, 0.0, 0.0002e18), &store); // 0.00002 WETH = $0.06
        assert!((s.tokens[2].usd - 0.03).abs() < 1e-9, "{}", s.tokens[2].usd);
        // …but takes over as soon as the deep pool is drained.
        s.apply(&sync(pool_b, 0.0, 0.0), &store);
        s.apply(&swap(pool_c, 10e18, 0.0, 0.0, 0.0002e18), &store);
        assert!((s.tokens[2].usd - 0.06).abs() < 1e-9, "{}", s.tokens[2].usd);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v4_amounts() {
        // Full-range-ish position in range: both tokens.
        let (a0, a1) = amounts(1.0, -1000, 1000, 1e6);
        assert!(a0 > 0.0 && a1 > 0.0 && (a0 - a1).abs() / a0 < 1e-9);
        assert_eq!(amounts(0.5, 0, 10, 1.0).1, 0.0, "below range: only token0");
    }
}
