//! In-memory index state: every token and pool, open candles and 24 h stats.
//! The indexer applies logs in chain order and flushes changes to the store in batches.

use crate::abi::{self, Addr, Event, Id, Kind, RawLog, WETH, hex20};
use crate::model::{Candle, Pool, RES, Stats, Token, Trade, TradeKind};
use crate::store::{Store, candle_key, trade_key};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

pub const NONE: u32 = u32::MAX;
/// Quote-side liquidity a pool needs before its trades may set a token's USD price.
const MIN_REF_LIQ: f64 = 1_000.0;
/// Real depth behind a quote token's price before its pairs' moves rank on Gainers and Losers.
const DEEP_QUOTE: f64 = 100_000.0;
/// Pricing pools followed when capping a pool's liquidity (see `quote_liq`).
const MAX_HOPS: usize = 8;
const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;

pub const STABLES: [Addr; 4] = [
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

    /// USD value of a pool's quote-side balance (0 when unknown), at most what the quote
    /// token's own pricing pool holds: those tokens can't be sold for more than that. Without
    /// the cap, fake tokens priced each other up hop by hop (a $56 pool became $>999T five
    /// pairs later). Real chains reach a stablecoin or WETH in a few hops; anyone can make a
    /// long one, so past `MAX_HOPS` it counts as nothing.
    fn quote_liq(&self, mut p: u32) -> f64 {
        let mut v = f64::INFINITY;
        for _ in 0..MAX_HOPS {
            let Some(pool) = self.pools.get(p as usize) else {
                return 0.0;
            };
            let q = &self.tokens[pool.quote() as usize];
            let here = pool.bal_quote() * q.usd;
            if !here.is_finite() {
                return 0.0;
            }
            v = v.min(here);
            if q.addr == WETH || q.ref_pool == NONE {
                return v;
            }
            p = q.ref_pool;
        }
        0.0
    }

    /// How much a token with a common symbol looks like the real one: the chain's own WETH and
    /// stablecoins first, then tokens with a listed icon. Ranking copies by liquidity alone
    /// puts scams on top, since a fake quote token can price its pool at billions.
    pub fn standing(&self, t: u32) -> (bool, bool) {
        let tok = &self.tokens[t as usize];
        (tok.addr == WETH || STABLES.contains(&tok.addr), tok.icon)
    }

    /// Whether pairs quoted in `t` may rank on Gainers and Losers: WETH, a stablecoin, or a token
    /// with at least $100k of real depth behind its price (its reference pool's capped quote
    /// side, half its liquidity). Whoever mints a junk quote token sets both its USD price and its pairs' prices in
    /// it for free (one tiny trade in its own pool, or overpaying with freshly minted tokens), so
    /// no formula over those two prices can tell a real move from a fake one.
    // ponytail: the depth is parked, not spent (in a v3/v4 pool it can even sit out of range), so
    // an owner who parks $100k behind a junk token still can. Measuring depth near the current
    // price would close that.
    pub fn deep_quote(&self, t: u32) -> bool {
        self.standing(t).0 || self.quote_liq(self.tokens[t as usize].ref_pool) >= DEEP_QUOTE
    }

    /// Pool liquidity in USD: twice the quote side. Valuing the base side at the pool's own
    /// price would let one trade at an absurd price inflate it without limit.
    pub fn liq(&self, p: u32) -> f64 {
        2.0 * self.quote_liq(p)
    }

    /// USD volume of a trade of `size` of a pool's quote token. A stablecoin or WETH counts at
    /// its price; any other token at no more than the depth behind its price (its reference
    /// pool's capped quote side). A junk token priced at $1e9 from $5k of real liquidity then
    /// adds at most $5k of volume per trade, the same as trading with that $5k, not
    /// `amount × $1e9`. Only volume (candles and 24 h stats, which rank the lists) is capped:
    /// a trade's own value stays price × amount, matching the price shown and wallet PnL.
    fn quote_value(&self, p: u32, size: f64) -> f64 {
        let quote = &self.tokens[self.pools[p as usize].quote() as usize];
        let v = size * quote.usd;
        if !v.is_finite() || quote.addr == WETH || STABLES.contains(&quote.addr) {
            return v;
        }
        v.min(self.quote_liq(quote.ref_pool).max(0.0))
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
            Event::SwapV2 { d0, d1 } => self.trade(p, log, ts, d0 / s0, d1 / s1, 1.0, store),
            Event::SwapCl { d0, d1, sqrt_price, fee } => {
                let (d0, d1) = (d0 / s0, d1 / s1);
                // LP fees stay in the pool until withdrawn; v4 withdrawals don't report
                // them, so leave them out of v4 balances. v3 Collect includes them.
                let keep = if pool.kind == Kind::V4 { 1.0 - fee as f64 / 1e6 } else { 1.0 };
                pool.bal0 += if d0 > 0.0 { d0 * keep } else { d0 };
                pool.bal1 += if d1 > 0.0 { d1 * keep } else { d1 };
                pool.sqrt_price = sqrt_price;
                self.trade(p, log, ts, d0, d1, keep, store)
            }
            Event::Flow { d0, d1 } => {
                pool.bal0 += d0 / s0;
                pool.bal1 += d1 / s1;
            }
            Event::Liquidity { d0, d1, add, moves } => {
                if moves {
                    pool.bal0 += d0 / s0;
                    pool.bal1 += d1 / s1;
                }
                self.liquidity(p, log, ts, d0 / s0, d1 / s1, add);
            }
            Event::Modify { tick_lower, tick_upper, liquidity } => {
                let (a0, a1) = amounts(pool.sqrt_price / Q96, tick_lower, tick_upper, liquidity);
                pool.bal0 += a0 / s0;
                pool.bal1 += a1 / s1;
                self.liquidity(p, log, ts, (a0 / s0).abs(), (a1 / s1).abs(), liquidity > 0.0);
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

    /// Records liquidity added or removed (whole tokens, positive) for the trades list.
    /// It isn't a trade: no candles, volume or price.
    fn liquidity(&mut self, p: u32, log: &RawLog, ts: u32, a0: f64, a1: f64, add: bool) {
        let pool = &self.pools[p as usize];
        let (base, quote) = if pool.quote0 { (a1, a0) } else { (a0, a1) };
        if base == 0.0 && quote == 0.0 {
            return;
        }
        let qusd = self.tokens[pool.quote() as usize].usd;
        let usd = quote * qusd + base * pool.price_usd;
        let trade = Trade {
            ts,
            tx: log.tx,
            maker: log.from,
            buy: false,
            base,
            quote,
            usd: if usd.is_finite() { usd as f32 } else { f32::NAN },
            price_usd: pool.price_usd as f32,
            price_native: pool.price_native as f32,
            kind: if add { TradeKind::Add } else { TradeKind::Remove },
        };
        self.trades.push((trade_key(p, log.block, log.index), trade.encode()));
        self.touched.insert(p);
    }

    /// Records a swap given the pool's balance deltas in whole tokens. The price is what
    /// was actually exchanged: a concentrated-liquidity pool's spot price after a swap
    /// through an empty range sits at the protocol limit, which nobody traded at. `keep` is
    /// the share of tokens paid in that the pool's balances counted (v4 leaves out fees).
    #[allow(clippy::too_many_arguments)]
    fn trade(&mut self, p: u32, log: &RawLog, ts: u32, d0: f64, d1: f64, keep: f64, store: &Store) {
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
        let vol = self.quote_value(p, dq.abs());
        let buy = db < 0.0;
        let trade = Trade {
            ts,
            tx: log.tx,
            maker: log.from,
            buy,
            base: db.abs(),
            quote: dq.abs(),
            usd: (dq.abs() * qusd) as f32,
            price_usd: usd as f32,
            price_native: native as f32,
            kind: TradeKind::Swap,
        };
        self.trades.push((trade_key(p, log.block, log.index), trade.encode()));
        self.touched.insert(p);
        // A launch can seed a pool with the base token alone and have its creator buy the
        // cheapest part of it, far below where anyone else can trade. So a pool's first USD
        // price comes from a trade that meets quote tokens already in the pool; one before
        // it is listed but sets no price, candle or volume. (Balances are already past this
        // trade, which added `dq`, or only `dq * keep` when paid in.)
        let before = pool.bal_quote() - if dq > 0.0 { dq * keep } else { dq };
        if !pool.price_usd.is_finite() && usd.is_finite() && before <= dq.abs() * 1e-3 {
            self.pools[p as usize].last_ts = ts;
            return;
        }
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

        // Liquidity added to MEME/WETH shows in its trades as an Add, without touching
        // candles or 24 h volume.
        let vol = s.stats[&2].window(s.head_ts, 86_400).vol;
        s.apply(&log(11, pool_b, vec![T.v2_mint, pad(&[7; 20])], [word(500e18), word(0.005e18)].concat()), &store);
        s.flush(&store, 12).unwrap();
        let last = &store.trades(2, (u32::MAX, 0), 1)[0].1;
        assert_eq!((last.kind, last.base, last.quote), (TradeKind::Add, 500.0, 0.005));
        assert_eq!(s.stats[&2].window(s.head_ts, 86_400).vol, vol);
        assert_eq!(store.candles(2, 0, 0, u32::MAX, 10)[0].1.buys + store.candles(2, 0, 0, u32::MAX, 10)[0].1.sells, 1);

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

        // A pool holding a trillion MEME is worth no more than MEME's own market ($3k of WETH).
        let (fake_tok, pool_d) = ([6u8; 20], [8u8; 20]);
        s.add_token(fake_tok, meta("FAKE", 18), 1);
        s.apply(&created(pool_d, fake_tok, meme), &store);
        s.apply(&sync(pool_d, 1e18, 1e12 * 1e18), &store);
        let d = s.pool_ix[&pad(&pool_d)];
        assert!((s.liq(d) - 6_000.0).abs() < 1.0, "capped: {}", s.liq(d));

        // A token whose price came from such a pool (stored before the cap) takes a real
        // price back at the next trade in a deeper USDC pool.
        let (late, pool_e, pool_f) = ([12u8; 20], [10u8; 20], [11u8; 20]);
        s.add_token(late, meta("LATE", 18), 1);
        s.apply(&created(pool_e, late, meme), &store);
        s.apply(&sync(pool_e, 1e18, 1000e18), &store);
        let (l, e) = (s.token_ix[&late], s.pool_ix[&pad(&pool_e)]);
        s.tokens[2].usd = 1e20; // MEME inflated: pool_e claims $1e23 of it
        (s.tokens[l as usize].usd, s.tokens[l as usize].ref_pool) = (1e23, e);
        s.apply(&created(pool_f, late, usdc), &store);
        s.apply(&sync(pool_f, 10_000e18, 10_000e6), &store);
        s.apply(&swap(pool_f, 0.0, 1e6, 1e18, 0.0), &store); // 1 LATE for 1 USDC
        assert!((s.tokens[l as usize].usd - 1.0).abs() < 1e-9, "{}", s.tokens[l as usize].usd);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two's complement of `word(x)`, for negative int256 amounts.
    fn neg(x: f64) -> Vec<u8> {
        let mut w = word(x);
        let mut carry = true;
        for b in w.iter_mut().rev() {
            (*b, carry) = (!*b).overflowing_add(carry as u8);
        }
        w
    }

    /// A launch seeds the pool with the token alone and its creator buys a quarter of the
    /// supply at a dust price in the same transaction (seen on Base as v4 launches showing
    /// +13M%). That trade is listed, but the pool's price, candles and changes start at the
    /// next one, also after a restart.
    #[test]
    fn creator_buy_into_token_only_pool_sets_no_baseline() {
        let dir = std::env::temp_dir().join(format!("ps-test-launch-{}", std::process::id()));
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let meme = [9u8; 20]; // token0, WETH token1
        s.add_token(WETH, meta("WETH", 18), 1);
        s.add_token(meme, meta("MEME", 18), 1);
        s.tokens[0].usd = 3000.0;
        let factory = hex20("33128a8fc17869897dce68ed026d694621f6fdfd");
        let pool = [2u8; 20];
        let at = |block: u32, topics: Vec<[u8; 32]>, data: Vec<u8>| log(block, pool, topics, data);
        let mut created = log(10, factory, vec![T.v3_pool_created, pad(&meme), pad(&WETH), [0; 32]], word(200.0));
        created.data.extend(pad(&pool));
        s.apply(&created, &store);
        let p = s.pool_ix[&pad(&pool)];
        // 2B MEME and no WETH, then the creator buys 500M MEME for 0.1 WETH ($6e-7 each).
        s.apply(
            &at(10, vec![T.v3_mint, [0; 32], [0; 32], [0; 32]], [word(0.0), word(1.0), word(2e27), word(0.0)].concat()),
            &store,
        );
        let swap = |block: u32, meme_out: f64, weth_in: f64| {
            at(
                block,
                vec![T.v3_swap, [0; 32], [0; 32]],
                [neg(meme_out), word(weth_in), word(1.0), word(1.0), word(0.0)].concat(),
            )
        };
        s.apply(&swap(10, 5e26, 1e17), &store);
        assert!(s.pools[p as usize].price_usd.is_nan(), "no price from the creator's buy");
        assert!(!s.stats.contains_key(&p) && !s.candles.contains_key(&(p, 0)), "nor candles or volume");
        // A minute later the market trades at $0.09: 10 MEME for 0.0003 WETH.
        s.apply(&swap(80, 1e19, 3e14), &store);
        assert!((s.pools[p as usize].price_usd - 0.09).abs() < 1e-9, "{}", s.pools[p as usize].price_usd);
        let checks = |s: &State| {
            let m = crate::pages::metrics(s, p);
            for w in 0..4 {
                assert!(crate::pages::change(&m, w).abs() < 1e-6, "window {w}: {}", crate::pages::change(&m, w));
            }
        };
        checks(&s);
        s.flush(&store, 81).unwrap();
        assert_eq!(store.trades(p, (u32::MAX, 0), 10).len(), 2, "the creator's buy is still listed");
        let candles = store.candles(p, 0, 0, u32::MAX, 10);
        assert_eq!(candles.len(), 1);
        assert!((candles[0].1.o as f64 - 0.09).abs() < 1e-6 && (candles[0].1.l as f64 - 0.09).abs() < 1e-6);
        checks(&State::load(&store).unwrap());
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The same launch on v4 with ETH as token0 and a 50% launch fee, restarted between the
    /// creator's buy and the next one. Fees paid in aren't in v4 balances, so the next buy,
    /// 20 times the ETH the pool held, still counts as meeting it.
    #[test]
    fn v4_launch_with_high_fee() {
        let dir = std::env::temp_dir().join(format!("ps-test-v4-launch-{}", std::process::id()));
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let meme = [9u8; 20];
        s.add_token(WETH, meta("WETH", 18), 1);
        s.add_token(meme, meta("MEME", 18), 1);
        s.tokens[0].usd = 3000.0;
        let id = [5u8; 32];
        let pm = |block: u32, topic: [u8; 32], data: Vec<Vec<u8>>| {
            log(block, abi::V4_POOL_MANAGER, vec![topic, id, [0; 32], pad(&meme)], data.concat())
        };
        // Starts at price 1 (tick 0) with MEME only in ticks -200..-100: ~2B MEME, no ETH.
        s.apply(&pm(10, T.v4_initialize, vec![word(500_000.0), word(1.0), word(0.0), word(Q96), word(0.0)]), &store);
        s.apply(&pm(10, T.v4_modify, vec![neg(200.0), neg(100.0), word(4e29), word(0.0)]), &store);
        let p = s.pool_ix[&id];
        assert!(s.pools[p as usize].quote0 && s.pools[p as usize].bal0 == 0.0);
        // v4 reports the swapper's deltas: pays ETH (negative), gets MEME.
        let buy = |block: u32, eth: f64, meme: f64| {
            pm(block, T.v4_swap, vec![neg(eth), word(meme), word(Q96), word(1.0), word(0.0), word(500_000.0)])
        };
        s.apply(&buy(10, 1e17, 5e26), &store); // the creator: 500M MEME for 0.1 ETH
        assert!(s.pools[p as usize].price_usd.is_nan());
        s.flush(&store, 11).unwrap();
        let mut s = State::load(&store).unwrap();
        s.apply(&buy(80, 1e18, 1e18 / 3e-5), &store); // 1 ETH at $0.09
        assert!((s.pools[p as usize].price_usd - 0.09).abs() < 1e-9, "{}", s.pools[p as usize].price_usd);
        let m = crate::pages::metrics(&s, p);
        for w in 0..4 {
            assert!(crate::pages::change(&m, w).abs() < 1e-6, "window {w}: {}", crate::pages::change(&m, w));
        }
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn junk_quote_token_cannot_take_over_a_price() {
        let dir = std::env::temp_dir().join(format!("ps-test-junk-{}", std::process::id()));
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let (usdc, junk, eurc) = (STABLES[0], [8u8; 20], [9u8; 20]);
        s.add_token(usdc, meta("USDC", 6), 1);
        s.add_token(junk, meta("BTCB", 18), 1); // seen before EURC, so it quotes EURC/BTCB
        s.add_token(eurc, meta("EURC", 6), 1);
        let uniswap = hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6");
        let created = |pool: Addr, t0: Addr, t1: Addr| {
            let mut data = pad(&pool).to_vec();
            data.extend(word(1.0));
            log(10, uniswap, vec![T.v2_pair_created, pad(&t0), pad(&t1)], data)
        };
        let sync = |pool: Addr, r0: f64, r1: f64| log(11, pool, vec![T.v2_sync], [word(r0), word(r1)].concat());
        let swap = |pool: Addr, a0in: f64, a1in: f64, a0out: f64, a1out: f64| {
            let data = [word(a0in), word(a1in), word(a0out), word(a1out)].concat();
            log(11, pool, vec![T.v2_swap, [0; 32], [0; 32]], data)
        };
        let (deep, thin, bad) = ([2u8; 20], [3u8; 20], [4u8; 20]);
        // EURC/USDC: 1M EURC against $1.1M. EURC = $1.10.
        s.apply(&created(deep, usdc, eurc), &store);
        s.apply(&sync(deep, 1_100_000e6, 1_000_000e6), &store);
        s.apply(&swap(deep, 110e6, 0.0, 0.0, 100e6), &store);
        assert!((s.tokens[2].usd - 1.1).abs() < 1e-9, "{}", s.tokens[2].usd);
        // BTCB/USDC holds $2k of real USDC; one trade prices BTCB at $1e9.
        s.apply(&created(thin, usdc, junk), &store);
        s.apply(&sync(thin, 2_000e6, 1_000_000e18), &store);
        s.apply(&swap(thin, 1_000e6, 0.0, 0.0, 1e12), &store);
        assert!((s.tokens[1].usd - 1e9).abs() < 1.0, "{}", s.tokens[1].usd);
        // EURC/BTCB: 21 EURC against 717 BTCB, "worth" $717B at BTCB's price, $2k really.
        s.apply(&created(bad, eurc, junk), &store);
        s.apply(&sync(bad, 21e6, 717e18), &store);
        assert!((s.liq(2) - 4_000.0).abs() < 1e-6, "valued by BTCB's real depth: {}", s.liq(2));
        s.apply(&swap(bad, 1e6, 0.0, 0.0, 34e18), &store); // 1 EURC for 34 BTCB = "$34B"
        assert!((s.tokens[2].usd - 1.1).abs() < 1e-9, "junk pool ignored: {}", s.tokens[2].usd);
        assert_eq!(s.tokens[2].ref_pool, 0);
        // A price the junk pool set before this fix gives way at the deep pool's next trade.
        (s.tokens[2].usd, s.tokens[2].ref_pool) = (34e9, 2);
        s.apply(&swap(deep, 110e6, 0.0, 0.0, 100e6), &store);
        assert!((s.tokens[2].usd - 1.1).abs() < 1e-9, "{}", s.tokens[2].usd);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn junk_quote_token_cannot_inflate_volume() {
        let dir = std::env::temp_dir().join(format!("ps-test-vol-{}", std::process::id()));
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let (usdc, j, x) = (STABLES[0], [8u8; 20], [9u8; 20]);
        s.add_token(usdc, meta("USDC", 6), 1);
        s.add_token(j, meta("J", 18), 1); // seen before X, so it quotes X/J
        s.add_token(x, meta("X", 18), 1);
        let uniswap = hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6");
        let created = |pool: Addr, t0: Addr, t1: Addr| {
            let mut data = pad(&pool).to_vec();
            data.extend(word(1.0));
            log(10, uniswap, vec![T.v2_pair_created, pad(&t0), pad(&t1)], data)
        };
        let sync = |pool: Addr, r0: f64, r1: f64| log(11, pool, vec![T.v2_sync], [word(r0), word(r1)].concat());
        let swap = |pool: Addr, a0in: f64, a1in: f64, a0out: f64, a1out: f64| {
            let data = [word(a0in), word(a1in), word(a0out), word(a1out)].concat();
            log(11, pool, vec![T.v2_swap, [0; 32], [0; 32]], data)
        };
        let (jusdc, xj) = ([2u8; 20], [3u8; 20]);
        // J/USDC holds $5k of real USDC; one tiny trade prices J at $1e9.
        s.apply(&created(jusdc, usdc, j), &store);
        s.apply(&sync(jusdc, 5_000e6, 1_000_000e18), &store);
        s.apply(&swap(jusdc, 1_000e6, 0.0, 0.0, 1e12), &store);
        assert!((s.tokens[1].usd - 1e9).abs() < 1.0, "{}", s.tokens[1].usd);
        // X/J with 0.00001 J passes the $10k list gate.
        s.apply(&created(xj, j, x), &store);
        s.apply(&sync(xj, 1e13, 1_000_000e18), &store);
        assert!((s.liq(1) - 10_000.0).abs() < 1e-6, "{}", s.liq(1));
        // A first trade meeting the J already in the pool sets its price (a first trade into a
        // pool holding no J would set none) and adds its $1.
        s.apply(&swap(xj, 1e9, 0.0, 0.0, 1e18), &store);
        let v0 = s.stats[&1].window(s.head_ts, 86_400).vol;
        assert!((v0 - 1.0).abs() < 1e-6, "{v0}");
        let vol = |s: &State| s.stats[&1].window(s.head_ts, 86_400).vol - v0;
        // Selling 1 J ("$1e9") counts for no more than the $5k behind J's price…
        s.apply(&sync(xj, 1e18 + 1e13, 500_000e18), &store);
        s.apply(&swap(xj, 1e18, 0.0, 0.0, 500_000e18), &store);
        let v1 = vol(&s);
        assert!(v1 > 4_999.0 && v1 <= 5_000.0, "{v1}");
        // The trade itself still shows price × amount, matching its price and wallet PnL.
        s.flush(&store, 12).unwrap();
        let sell = &store.trades(1, (u32::MAX, 0), 1)[0].1;
        assert_eq!((sell.quote, sell.usd), (1.0, 1e9));
        // …and neither does buying the whole J side back out.
        s.apply(&sync(xj, 1e6, 1_000_000e18), &store);
        s.apply(&swap(xj, 0.0, 500_000e18, 1e18 + 1e13 - 1e6, 0.0), &store);
        let v2 = vol(&s) - v1;
        assert!(v2 > 4_999.0 && v2 <= 5_000.0 + 1e-6, "{v2}");
        // A pool whose tracked quote balance drifted to 0 still records its trades.
        s.apply(&sync(xj, 0.0, 1_000_000e18), &store);
        s.apply(&swap(xj, 1e9, 0.0, 0.0, 1e18), &store);
        let v3 = vol(&s) - v1 - v2;
        assert!((v3 - 1.0).abs() < 1e-6, "{v3}");

        // A real quote token keeps its full volume, even in a pool holding more of it than
        // its own reference pool has behind it: Q at $1 from $8k of USDC, Y/Q with 40k Q.
        let (q, y, qusdc, yq) = ([10u8; 20], [11u8; 20], [4u8; 20], [5u8; 20]);
        s.add_token(q, meta("Q", 18), 1);
        s.add_token(y, meta("Y", 18), 1);
        s.apply(&created(qusdc, usdc, q), &store);
        s.apply(&sync(qusdc, 8_000e6, 8_000e18), &store);
        s.apply(&swap(qusdc, 1e6, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[3].usd - 1.0).abs() < 1e-9, "{}", s.tokens[3].usd);
        assert_eq!(s.stats[&2].window(s.head_ts, 86_400).vol, 1.0, "USDC-quoted volume as traded");
        s.apply(&created(yq, q, y), &store);
        s.apply(&sync(yq, 40_100e18, 1_000e18), &store);
        s.apply(&swap(yq, 100e18, 0.0, 0.0, 2e18), &store);
        let vq = s.stats[&3].window(s.head_ts, 86_400).vol;
        assert!((vq - 100.0).abs() < 1e-6, "{vq}");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn junk_quote_token_cannot_top_gainers_or_losers() {
        use crate::lists::{Filter, ranked};
        let dir = std::env::temp_dir().join(format!("ps-test-gainers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let usdc = STABLES[0];
        let (j, q, j2) = ([8u8; 20], [9u8; 20], [14u8; 20]);
        let (x, y, v, w, u, z) = ([10u8; 20], [11u8; 20], [12u8; 20], [13u8; 20], [15u8; 20], [18u8; 20]);
        s.add_token(usdc, meta("USDC", 6), 1);
        // J and J2 (junk) and Q (deep) are seen first, so they quote their pairs.
        for t in [j, q, j2, x, y, v, w, u, z] {
            s.add_token(t, meta("T", 18), 1);
        }
        let uniswap = hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6");
        let created = |pool: Addr, t0: Addr, t1: Addr| {
            let mut data = pad(&pool).to_vec();
            data.extend(word(1.0));
            log(10, uniswap, vec![T.v2_pair_created, pad(&t0), pad(&t1)], data)
        };
        let sync = |pool: Addr, r0: f64, r1: f64| log(11, pool, vec![T.v2_sync], [word(r0), word(r1)].concat());
        let swap = |pool: Addr, a0in: f64, a1in: f64, a0out: f64, a1out: f64| {
            let data = [word(a0in), word(a1in), word(a0out), word(a1out)].concat();
            log(11, pool, vec![T.v2_swap, [0; 32], [0; 32]], data)
        };
        // Pools in creation order: J/USDC 0, Q/USDC 1, X/J 2, Y/J 3, W/Q 4, V/J 5, J2/Q 6, U/J2 7, Z/USDC 8.
        let (jusdc, qusdc, xj, yj, wq, vj) = ([2u8; 20], [3u8; 20], [4u8; 20], [5u8; 20], [6u8; 20], [7u8; 20]);
        let (j2q, uj2, zusdc) = ([16u8; 20], [17u8; 20], [19u8; 20]);
        // J/USDC holds $12.6k of real USDC, J trades at $0.0125. Q/USDC holds $300k, Q at $1.
        s.apply(&created(jusdc, usdc, j), &store);
        s.apply(&sync(jusdc, 12_600e6, 1_000_000e18), &store);
        s.apply(&swap(jusdc, 1e6, 0.0, 0.0, 80e18), &store);
        s.apply(&created(qusdc, usdc, q), &store);
        s.apply(&sync(qusdc, 300_000e6, 300_000e18), &store);
        s.apply(&swap(qusdc, 1e6, 0.0, 0.0, 1e18), &store);
        assert_eq!((s.tokens[1].usd, s.tokens[2].usd), (0.0125, 1.0));
        // X/J and Y/J: 2M J each, $25.2k of capped liquidity, $12.5k of volume at 1 J.
        // W/Q: 20k Q, $40k of liquidity, $20k of volume at 1 Q.
        for (pool, quote, base, n) in [(xj, j, x, 1e6), (yj, j, y, 1e6), (wq, q, w, 2e4)] {
            s.apply(&created(pool, quote, base), &store);
            s.apply(&swap(pool, 0.0, n * 1e18, n * 1e18, 0.0), &store);
            s.apply(&sync(pool, 2.0 * n * 1e18, n * 1e18), &store);
        }
        // J's minter overpays in X/J with freshly minted J: X is "up 1e15×" in J and in USD.
        s.apply(&swap(xj, 1e33, 0.0, 0.0, 1e18), &store);
        // W really rises 50% against Q.
        s.apply(&swap(wq, 1.5e18, 0.0, 0.0, 1e18), &store);
        // The ticket's attack: one tiny J/USDC trade prices J at $1e9, and Y/J trades flat.
        s.apply(&swap(jusdc, 1_000e6, 0.0, 0.0, 1e12), &store);
        assert!((s.tokens[1].usd - 1e9).abs() < 1.0, "{}", s.tokens[1].usd);
        s.apply(&swap(yj, 0.0, 1e18, 1e18, 0.0), &store);
        // V/J opens at J's pumped price.
        s.apply(&created(vj, j, v), &store);
        s.apply(&sync(vj, 2_000_000e18, 1_000_000e18), &store);
        s.apply(&swap(vj, 0.0, 1e18, 1e18, 0.0), &store);
        // J2 is priced through the deep Q, but only $12.6k of Q backs it; the same pump on U/J2.
        s.apply(&created(j2q, q, j2), &store);
        s.apply(&sync(j2q, 12_600e18, 1_000_000e18), &store);
        s.apply(&swap(j2q, 1e18, 0.0, 0.0, 80e18), &store);
        s.apply(&created(uj2, j2, u), &store);
        s.apply(&swap(uj2, 0.0, 1e24, 1e24, 0.0), &store);
        s.apply(&sync(uj2, 2e24, 1e24), &store);
        s.apply(&swap(j2q, 1_000e18, 0.0, 0.0, 1e12), &store);
        s.apply(&swap(uj2, 0.0, 1e18, 1e18, 0.0), &store);
        // Z/USDC: 20k Z sold at $1 ($20k of volume, $80k of liquidity), then Z really falls 50%.
        s.apply(&created(zusdc, usdc, z), &store);
        s.apply(&swap(zusdc, 0.0, 2e22, 2e10, 0.0), &store);
        s.apply(&sync(zusdc, 4e10, 2e22), &store);
        s.apply(&swap(zusdc, 0.0, 2e18, 1e6, 0.0), &store);
        let list = |s: &State, tab: &str| ranked(s, &Filter { tab: Some(tab.into()), ..Default::default() });
        let ch = |s: &State, p: u32| crate::pages::change(&crate::pages::metrics(s, p), 3);
        let check = |s: &State| {
            for p in [2, 3, 4, 5, 7, 8] {
                let m = crate::pages::metrics(s, p);
                assert!(m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0, "pool {p} passes the list gates");
            }
            assert!(ch(s, 2) > 1e14 && ch(s, 3) > 1e10 && ch(s, 7) > 1e10, "{} {} {}", ch(s, 2), ch(s, 3), ch(s, 7));
            assert_eq!(list(s, "gainers"), vec![4], "only W/Q's real 50% ranks");
            assert_eq!(list(s, "losers"), vec![8], "only Z/USDC's real 50% fall ranks");
            // Sorting any list by change, price or FDV ranks pairs quoted in J or J2 last, either way.
            let cols = [("c24h", None), ("c5m", Some("asc")), ("price", None), ("fdv", None), ("price", Some("asc"))];
            for (col, dir) in cols {
                let f = Filter { sort: Some(col.into()), dir: dir.map(Into::into), ..Default::default() };
                let mut pools: Vec<u32> = (0..9).collect();
                crate::lists::sort(s, &f, &mut pools);
                let (head, tail) = pools.split_at(5);
                let sorted = |p: &[u32]| {
                    let mut p = p.to_vec();
                    p.sort();
                    p
                };
                assert_eq!((sorted(head), sorted(tail)), (vec![0, 1, 4, 6, 8], vec![2, 3, 5, 7]), "{col} {pools:?}");
            }
            // J/USDC and J2/Q still top a price sort: J and J2 are their base, priced through a real
            // stablecoin or Q pool, however thin. Only the quote token is gated here (see `lists::sort`).
            let f = Filter { sort: Some("price".into()), ..Default::default() };
            let mut pools: Vec<u32> = (0..9).collect();
            crate::lists::sort(s, &f, &mut pools);
            let top = if s.tokens[1].usd > 1.0 { [0, 6] } else { [6, 4] };
            assert_eq!(pools[..2], top, "{pools:?}");
        };
        check(&s);
        // J falls back to $0.0125: V/J is "down 100%".
        s.apply(&swap(jusdc, 0.0, 80e18, 1e6, 0.0), &store);
        s.apply(&swap(vj, 0.0, 1e18, 1e18, 0.0), &store);
        assert!(ch(&s, 5) < -0.99, "{}", ch(&s, 5));
        check(&s);
        // Stats rebuilt from stored candles rank the same.
        s.flush(&store, 12).unwrap();
        let s = State::load(&store).unwrap();
        check(&s);
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
