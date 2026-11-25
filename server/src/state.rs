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
/// Real depth behind a quote token's price before its pairs' moves rank on Gainers and Losers
/// and its pairs count in full for a token's price (see `ref_weight`).
const DEEP_QUOTE: f64 = 100_000.0;
/// How many times less a pool quoted in a token that isn't a `deep_quote` counts for a price.
// ponytail: a margin, not a proof, and every balance here can be borrowed for one transaction.
// Taking a price from a pool of depth D quoted in a deep token takes over 4×D behind a token
// someone controls (over D once $100k clears DEEP_QUOTE); taking one from a pool of depth D
// quoted in a shallower token takes a deep-quoted pool over D/4, plus a dip of that token's
// own pool under the bar when it sits just above it. Retune, or ramp the divisor with that
// pool's depth, if prod shows abuse either way.
const UNPROVEN_DIV: f64 = 4.0;
/// Pricing pools followed when capping a pool's liquidity (see `quote_liq`).
const MAX_HOPS: usize = 8;
/// Quote-side depth a swap needs behind its price move for that move to count on Gainers and
/// Losers: half their $25k liquidity floor, so as deep as a $25k x·y pool.
const MIN_BACKED: f64 = 12_500.0;
/// How far a v2 or Solidly trade's price may sit outside the pool's spot prices before and
/// after it (fees up to 3%, plus Aerodrome sending its fees out of the reserves).
const SPOT_SLACK: f64 = 1.05;
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
            thin_ts: 0,
            icon: false,
        });
        self.token_ix.insert(addr, idx);
        self.dirty_tokens.insert(idx);
    }

    pub fn set_supply(&mut self, token: u32, supply: f64) {
        self.tokens[token as usize].supply = supply;
        self.dirty_tokens.insert(token);
    }

    pub fn set_stable(&mut self, p: u32, stable: bool) {
        self.pools[p as usize].stable = Some(stable);
        self.dirty_pools.insert(p);
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
            let here = self.near_quote(pool) * q.usd;
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

    /// A pool's weight in the contest for its base token's price: its quote-side depth, divided
    /// by `UNPROVEN_DIV` unless the quote token is a `deep_quote`. Whoever makes a token can
    /// make its pricing pool look any way they like, so its capped depth proves little: $2k
    /// parked behind a junk token, out of anyone else's reach, let a pool holding dust of it
    /// outbid a small token's $1.5k USDC pool and price it at $100,000. Zeroing such pools
    /// instead would hand the price of a token traded mostly against a small token to any $1k
    /// USDC pool.
    fn ref_weight(&self, p: u32) -> f64 {
        let Some(pool) = self.pools.get(p as usize) else {
            return 0.0;
        };
        let liq = self.quote_liq(p);
        if self.deep_quote(pool.quote()) { liq } else { liq / UNPROVEN_DIV }
    }

    /// Quote tokens a pool holds near its price. For concentrated liquidity that is at most what
    /// an x·y pool with the same in-range liquidity would hold (L·√P): quote parked in a range
    /// far below the price, or dust liquidity at an absurd price, buys nothing at the price shown.
    /// ponytail: in-range liquidity is only known at the current tick, so a one-tick position
    /// can still stand in for depth; tracking every position's range would close that.
    fn near_quote(&self, pool: &Pool) -> f64 {
        let bal = pool.bal_quote();
        if !matches!(pool.kind, Kind::V3 | Kind::V4) || !pool.liquidity.is_finite() || pool.sqrt_price <= 0.0 {
            return bal;
        }
        bal.min(self.at_price(pool, pool.liquidity))
    }

    /// Quote tokens an x·y pool with liquidity `l` at a concentrated-liquidity pool's price holds.
    fn at_price(&self, pool: &Pool, l: f64) -> f64 {
        let sp = pool.sqrt_price / Q96;
        let virt = if pool.quote0 { l / sp / self.scale(pool.token0) } else { l * sp / self.scale(pool.token1) };
        virt.max(0.0)
    }

    /// Quote per base at a concentrated-liquidity pool's √price (X96), in whole tokens; NaN
    /// before the price is known.
    fn cl_price(&self, pool: &Pool, sqrt: f64) -> f64 {
        if sqrt <= 0.0 {
            return f64::NAN;
        }
        let p1per0 = (sqrt / Q96).powi(2) * self.scale(pool.token0) / self.scale(pool.token1);
        if pool.quote0 { 1.0 / p1per0 } else { p1per0 }
    }

    /// How much a token with a common symbol looks like the real one: the chain's own WETH and
    /// stablecoins first, then tokens with a listed icon. Ranking copies by liquidity alone
    /// puts scams on top, since a fake quote token can price its pool at billions.
    pub fn standing(&self, t: u32) -> (bool, bool) {
        let tok = &self.tokens[t as usize];
        (tok.addr == WETH || STABLES.contains(&tok.addr), tok.icon)
    }

    /// Whether pairs quoted in `t` may rank on Gainers and Losers and count in full for a price:
    /// WETH, a stablecoin, or a token with at least $100k of real depth behind its price (its
    /// reference pool's capped quote side, half its liquidity). Whoever mints a junk quote token sets both its USD price and its pairs' prices in
    /// it for free (one tiny trade in its own pool, or overpaying with freshly minted tokens), so
    /// no formula over those two prices can tell a real move from a fake one.
    // ponytail: the depth is parked, not spent. A v3/v4 pool's counts only at the price (see
    // `near_quote`), but a one-tick position there still stands in for it; tracking every
    // position's range would close that.
    pub fn deep_quote(&self, t: u32) -> bool {
        self.standing(t).0 || self.quote_liq(self.tokens[t as usize].ref_pool) >= DEEP_QUOTE
    }

    /// Pool liquidity in USD: twice the quote side. Valuing the base side at the pool's own
    /// price would let one trade at an absurd price inflate it without limit.
    pub fn liq(&self, p: u32) -> f64 {
        2.0 * self.quote_liq(p)
    }

    /// How a token picks its default pair: a pool of at least $1k quoted in a `deep_quote`
    /// token first, then by liquidity. `liq` alone would let 0.001 of a junk token with $2k
    /// behind it rank at $4k and chart that pool's price, which the junk's minter sets.
    pub fn pair_rank(&self, p: u32) -> (bool, f64) {
        let liq = self.liq(p);
        (liq >= 1_000.0 && self.deep_quote(self.pools[p as usize].quote()), liq)
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
        if let Event::Created { kind, token0, token1, fee, sqrt_price, tick, origin, stable, .. } = ev {
            return self.create(id, kind, token0, token1, fee, (sqrt_price, tick), origin, stable, log.block, ts);
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
            Event::SwapV2 { d0, d1 } => {
                // Sync came first, so the balances are already past this swap.
                let (d0, d1) = (d0 / s0, d1 / s1);
                let spot = |b0: f64, b1: f64| if pool.quote0 { b0 / b1 } else { b1 / b0 };
                let spot = (spot(pool.bal0 - d0, pool.bal1 - d1), spot(pool.bal0, pool.bal1));
                self.trade(p, log, ts, d0, d1, 1.0, spot, f64::NAN, store)
            }
            Event::SwapCl { d0, d1, sqrt_price, liquidity, tick, fee } => {
                let (d0, d1) = (d0 / s0, d1 / s1);
                // LP fees stay in the pool until withdrawn; v4 withdrawals don't report
                // them, so leave them out of v4 balances. v3 Collect includes them.
                let keep = if pool.kind == Kind::V4 { 1.0 - fee as f64 / 1e6 } else { 1.0 };
                pool.bal0 += if d0 > 0.0 { d0 * keep } else { d0 };
                pool.bal1 += if d1 > 0.0 { d1 * keep } else { d1 };
                let (before, liq_before) = (pool.sqrt_price, pool.liquidity);
                pool.sqrt_price = sqrt_price;
                pool.liquidity = liquidity;
                pool.tick = tick as f64;
                let pool = &self.pools[p as usize];
                let spot = (self.cl_price(pool, before), self.cl_price(pool, sqrt_price));
                self.trade(p, log, ts, d0, d1, keep, spot, liq_before, store)
            }
            Event::Flow { d0, d1 } => {
                pool.bal0 += d0 / s0;
                pool.bal1 += d1 / s1;
            }
            Event::Liquidity { d0, d1, add, moves, range } => {
                if moves {
                    pool.bal0 += d0 / s0;
                    pool.bal1 += d1 / s1;
                }
                if let Some((lower, upper, l)) = range {
                    pool.shift_liquidity(lower, upper, l);
                }
                self.liquidity(p, log, ts, d0 / s0, d1 / s1, add);
            }
            Event::Modify { tick_lower, tick_upper, liquidity } => {
                let (a0, a1) = amounts(pool.sqrt_price / Q96, tick_lower, tick_upper, liquidity);
                pool.bal0 += a0 / s0;
                pool.bal1 += a1 / s1;
                pool.shift_liquidity(tick_lower, tick_upper, liquidity);
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
        (sqrt, tick): (f64, i32),
        origin: Addr,
        stable: bool,
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
            // A v4 pool starts empty at a known price; a v3 pool's price arrives with its first swap.
            liquidity: if kind == Kind::V4 { 0.0 } else { f64::NAN },
            tick: if kind == Kind::V4 { tick as f64 } else { f64::NAN },
            stable: (kind == Kind::Solidly).then_some(stable),
            spot: f64::NAN,
            thin_ts: 0,
        };
        if sqrt > 0.0 {
            pool.price_native = self.cl_price(&pool, sqrt);
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

    /// Records a swap given the pool's balance deltas in whole tokens and its spot price
    /// (quote per base) before and after. The price is what was actually exchanged: a
    /// concentrated-liquidity pool's spot price after a swap through an empty range sits at the
    /// protocol limit, which nobody traded at. `keep` is the share of tokens paid in that the
    /// pool's balances counted (v4 leaves out fees). `liq_before`: a concentrated-liquidity
    /// pool's in-range liquidity before the swap (NaN if unknown or x·y).
    #[allow(clippy::too_many_arguments)]
    fn trade(
        &mut self,
        p: u32,
        log: &RawLog,
        ts: u32,
        d0: f64,
        d1: f64,
        keep: f64,
        spot: (f64, f64),
        liq_before: f64,
        store: &Store,
    ) {
        let pool = &self.pools[p as usize];
        let (db, dq) = if pool.quote0 { (d1, d0) } else { (d0, d1) };
        if db == 0.0 || dq == 0.0 {
            // A swap that exchanged nothing still moved a concentrated pool's price. A pool
            // stored before we kept `spot` takes it from before this move, so the next trade
            // measures the move from there (`thin`). (Stable Solidly `spot` is curve-adjusted.)
            if matches!(pool.kind, Kind::V3 | Kind::V4)
                && pool.spot.is_nan()
                && pool.price_native.is_finite()
                && spot.0.is_finite()
            {
                self.pools[p as usize].spot = spot.0;
            }
            return;
        }
        let (base, quote) = (pool.base(), pool.quote());
        let exec = dq.abs() / db.abs();
        if !exec.is_finite() || exec <= 0.0 {
            return;
        }
        let native = match pool.kind {
            Kind::Solidly => clamp_spot(exec, spot, pool.stable == Some(true)),
            // Concentrated liquidity can't be overpaid, but a v4 fee (up to 100%, paid to the LP)
            // and rounding on a swap of a few wei both price a trade far off the curve.
            _ => clamp_spot(exec, spot, false),
        };
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
            price_usd: (exec * qusd) as f32,
            price_native: exec as f32,
            kind: TradeKind::Swap,
        };
        self.trades.push((trade_key(p, log.block, log.index), trade.encode()));
        self.touched.insert(p);
        // A launch can seed a pool with the base token alone and have its creator buy the
        // cheapest part of it, far below where anyone else can trade. So a pool's first USD
        // price comes from a trade that meets quote tokens already in the pool; one before
        // it is listed but sets no price, candle or volume. (Balances are already past this
        // trade, which added `dq`, or only `dq * keep` when paid in.)
        let curve_dq = if dq > 0.0 { dq * keep } else { dq };
        let before = pool.bal_quote() - curve_dq;
        if !pool.price_usd.is_finite() && usd.is_finite() && before <= dq.abs() * 1e-3 {
            self.pools[p as usize].last_ts = ts;
            return;
        }
        let (prev_usd, prev_native) = (pool.price_usd, pool.price_native);
        let (thin, spot_now) = self.thin(p, curve_dq, before, spot, native, liq_before);

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
            entry.1.add(usd, native, vol, buy, thin);
            self.dirty_candles.insert((p, ri));
        }
        let open = if prev_usd.is_finite() { prev_usd } else { usd };
        self.stats.entry(p).or_insert_with(Stats::new).add(ts, open, vol, buy, thin);

        let pool = &mut self.pools[p as usize];
        pool.price_native = native;
        pool.price_usd = usd;
        pool.last_ts = ts;
        // The next move is measured from here. After a thin trade that is the price it recorded,
        // so whatever gap it left to the real spot price counts against the next trade too.
        pool.spot = if thin || !spot_now.is_finite() { native } else { spot_now };
        if thin {
            pool.thin_ts = ts;
        }
        let pool_thin = pool.thin_ts;
        let trusted = pool.trusted();
        // The heaviest pool (`ref_weight`) sets the price, judged by liquidity now (a drained or
        // rugged reference pool loses its claim at once).
        let qliq = self.quote_liq(p);
        let tok = &self.tokens[base as usize];
        if trusted
            && !STABLES.contains(&tok.addr)
            && usd.is_finite()
            && qliq >= MIN_REF_LIQ
            && (tok.ref_pool == p || !tok.usd.is_finite() || self.ref_weight(p) > self.ref_weight(tok.ref_pool))
        {
            let tok = &mut self.tokens[base as usize];
            tok.usd = usd;
            tok.ref_pool = p;
            // A pool pumped thin while it wasn't the token's pricing pool brings that along
            // when it takes over.
            tok.thin_ts = tok.thin_ts.max(pool_thin);
            self.dirty_tokens.insert(base);
        }
        // A token links to its best-ranked (`pair_rank`) trusted pool, or any pool if it has no
        // trusted one: an unknown contract reports its own balances.
        let rank = |s: &State, q: u32| (s.pools[q as usize].trusted(), s.pair_rank(q));
        for t in [base, quote] {
            let best = self.tokens[t as usize].best_pool;
            if best != p && (best == NONE || rank(self, p) > rank(self, best)) {
                self.tokens[t as usize].best_pool = p;
                self.dirty_tokens.insert(t);
            }
        }
    }

    /// Whether a swap moved the price with less than `MIN_BACKED` of quote depth behind it, and
    /// the pool's spot price after it. The depth is the quote traded over the relative change in
    /// √price: an x·y pool's quote reserve, or L·√P for concentrated liquidity, met along the
    /// way. So a pump through dust liquidity or a nearly empty pool shows as thin even if deep
    /// liquidity is added at the new price afterwards. The move runs from the spot price the
    /// pool's last recorded trade left (swaps that exchanged nothing, or reserves changed by a
    /// bare Sync, count as moved by this trade) to the spot price now, and to this trade's own
    /// price past fee slack. `dq` is the quote that reached the curve (not a v4 fee). A move too
    /// small to measure counts the depth at the price, with the lower of the in-range liquidity
    /// before and after the swap (`liq_before`).
    fn thin(&self, p: u32, dq: f64, before: f64, spot: (f64, f64), native: f64, liq_before: f64) -> (bool, f64) {
        let pool = &self.pools[p as usize];
        let curve = |r: f64| if pool.stable == Some(true) { stable_price(r) } else { r };
        let to = curve(spot.1);
        let from = if pool.spot.is_finite() {
            pool.spot
        } else {
            // Stored before we kept `spot`: the last recorded price, if x·y reserves moved away
            // from it without a trade (a bare Sync). A concentrated-liquidity price only moves in
            // swaps, and a v4 pool's last recorded price includes its fee.
            let (s0, last) = (curve(spot.0), pool.price_native);
            let ok = |x: f64| x.is_finite() && x > 0.0;
            let xy = matches!(pool.kind, Kind::V2 | Kind::Solidly);
            if ok(last) && (!ok(s0) || xy && (last / s0).max(s0 / last) > SPOT_SLACK) { last } else { s0 }
        };
        let lo = from.min(to).min(native * SPOT_SLACK);
        let hi = from.max(to).max(native / SPOT_SLACK);
        let rel = if hi > lo { (hi / lo).sqrt() - 1.0 } else { 0.0 };
        let mut depth = if rel > 1e-4 {
            self.quote_value(p, dq.abs()) / rel
        } else {
            // A few wei through dust into a big position just past the price would otherwise
            // read as that position's depth, one tick at a time.
            // ponytail: only this swap's liquidity before counts, so a step entered by a swap that
            // exchanged nothing, or rolled from one one-tick position into the next, still passes;
            // that is the one-tick ceiling in `near_quote`.
            let l = liq_before.min(pool.liquidity);
            let v = if l.is_finite() {
                self.at_price(pool, l) * self.tokens[pool.quote() as usize].usd
            } else {
                f64::INFINITY
            };
            self.quote_liq(p).min(v)
        };
        // Overpaying moves x·y reserves further than the curve would, which would read as
        // depth; an x·y pool is never deeper than its quote reserve before the trade.
        if matches!(pool.kind, Kind::V2 | Kind::Solidly) {
            depth = depth.min(self.quote_value(p, before.max(0.0)));
        }
        if depth.is_nan() || depth < MIN_BACKED {
            return (true, to);
        }
        // A pair's USD price also moves with its quote token's. If a pool with a thin swap set
        // the quote's price since this pool last traded, the move isn't backed either, however
        // little the quote moved in between: a pump split into small steps adds up. WETH and
        // stablecoins are priced from deep pools.
        let q = &self.tokens[pool.quote() as usize];
        let anchor = q.addr == WETH || STABLES.contains(&q.addr);
        (!anchor && q.thin_ts >= pool.last_ts && q.thin_ts > 0, to)
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

/// A trade's price, kept within the pool's spot prices (quote per base) before and after it,
/// give or take fee slack. An x·y pool takes more than its curve asks for and keeps the surplus,
/// so one trade paying 1,000 USDC for a speck of a token would otherwise set its price a billion
/// times higher while the reserves barely move. A v4 hook's fee (up to 100%, paid to the LP) and
/// rounding on a swap of a few wei do the same in concentrated liquidity. `stable`: the stable
/// curve (x³y + y³x), which prices between the reserve ratio and 1:1.
fn clamp_spot(exec: f64, spot: (f64, f64), stable: bool) -> f64 {
    let (mut lo, mut hi) = (f64::INFINITY, 0.0f64);
    for r in [spot.0, spot.1].into_iter().filter(|r| r.is_finite() && *r >= 0.0) {
        let v = if stable { stable_price(r) } else { r };
        (lo, hi) = (lo.min(v), hi.max(v));
    }
    if hi > 0.0 { exec.clamp(lo / SPOT_SLACK, hi * SPOT_SLACK) } else { exec }
}

/// Marginal price on the stable curve (x³y + y³x) at reserve ratio `r` (quote per base).
fn stable_price(r: f64) -> f64 {
    r * (3.0 + r * r) / (1.0 + 3.0 * r * r)
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

    fn v2_sync(pool: Addr, r0: f64, r1: f64) -> RawLog {
        log(11, pool, vec![T.v2_sync], [word(r0), word(r1)].concat())
    }

    fn v2_swap(pool: Addr, a0in: f64, a1in: f64, a0out: f64, a1out: f64) -> RawLog {
        log(11, pool, vec![T.v2_swap, [0; 32], [0; 32]], [word(a0in), word(a1in), word(a0out), word(a1out)].concat())
    }

    /// A Uniswap v2 pool holding `r0` and `r1` whole tokens.
    fn v2_pool(s: &mut State, store: &Store, pool: Addr, t0: Addr, t1: Addr, r0: f64, r1: f64) {
        let uniswap = hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6");
        let data = [pad(&pool).to_vec(), word(1.0)].concat();
        s.apply(&log(10, uniswap, vec![T.v2_pair_created, pad(&t0), pad(&t1)], data), store);
        let (s0, s1) = (s.scale(s.token_ix[&t0]), s.scale(s.token_ix[&t1]));
        s.apply(&v2_sync(pool, r0 * s0, r1 * s1), store);
    }

    fn reserves(s: &State, pool: Addr) -> (f64, f64) {
        let p = &s.pools[s.pool_ix[&pad(&pool)] as usize];
        (p.bal0, p.bal1)
    }

    /// `l` moved to `block` (one second each).
    fn at(mut l: RawLog, block: u32) -> RawLog {
        l.ts = l.ts - l.block + block;
        l.block = block;
        l
    }

    fn xy(s: &mut State, store: &Store, pool: Addr, in0: f64, in1: f64) {
        xy_at(s, store, pool, in0, in1, 11)
    }

    /// An x·y trade paying in `in0` of token0 or `in1` of token1 (whole tokens) at `block`,
    /// Sync first as on chain.
    fn xy_at(s: &mut State, store: &Store, pool: Addr, in0: f64, in1: f64, block: u32) {
        let p = &s.pools[s.pool_ix[&pad(&pool)] as usize];
        let (r0, r1, s0, s1) = (p.bal0, p.bal1, s.scale(p.token0), s.scale(p.token1));
        let (out0, out1) = if in0 > 0.0 { (0.0, r1 * in0 / (r0 + in0)) } else { (r0 * in1 / (r1 + in1), 0.0) };
        s.apply(&at(v2_sync(pool, (r0 + in0 - out0) * s0, (r1 + in1 - out1) * s1), block), store);
        s.apply(&at(v2_swap(pool, in0 * s0, in1 * s1, out0 * s0, out1 * s1), block), store);
    }

    fn wash(s: &mut State, store: &Store, pool: Addr, q: f64) {
        wash_at(s, store, pool, q, 11)
    }

    /// Buys with `q` of token0 at `block`, then sells what it bought back.
    fn wash_at(s: &mut State, store: &Store, pool: Addr, q: f64, block: u32) {
        let r1 = reserves(s, pool).1;
        xy_at(s, store, pool, q, 0.0, block);
        let bought = r1 - reserves(s, pool).1;
        xy_at(s, store, pool, 0.0, bought, block);
    }

    fn m_thin(s: &State, p: u32) -> u32 {
        crate::pages::metrics(s, p).w[3].thin
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
                [neg(meme_out), word(weth_in), word(Q96 * (weth_in / meme_out).sqrt()), word(1.0), word(0.0)].concat(),
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
        // The pool's price after the trade: what was paid, before the 50% fee.
        let buy = |block: u32, eth: f64, meme: f64| {
            let sqrt = Q96 * (meme * 2.0 / eth).sqrt();
            pm(
                block,
                T.v4_swap,
                vec![neg(eth), word(meme), word(sqrt), word(1.0), tick_at(sqrt / Q96), word(500_000.0)],
            )
        };
        s.apply(&buy(10, 1e17, 5e26), &store); // the creator: 500M MEME for 0.1 ETH
        assert!(s.pools[p as usize].price_usd.is_nan());
        s.flush(&store, 11).unwrap();
        let mut s = State::load(&store).unwrap();
        s.apply(&buy(80, 1e18, 1e18 / 3e-5), &store); // 1 ETH at $0.09, half of it the fee
        // The pool's price counts the fee only up to fee slack: $0.045 × 1.05.
        assert!((s.pools[p as usize].price_usd - 0.04725).abs() < 1e-9, "{}", s.pools[p as usize].price_usd);
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
        // BTCB/USDC holds $2k of real USDC against 0.000002 BTCB: BTCB at $1e9.
        s.apply(&created(thin, usdc, junk), &store);
        s.apply(&sync(thin, 2_000e6, 2e12), &store);
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
        // J/USDC holds $5k of real USDC against 0.000005 J: J at $1e9.
        s.apply(&created(jusdc, usdc, j), &store);
        s.apply(&sync(jusdc, 5_000e6, 5e12), &store);
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
    fn junk_quote_token_cannot_be_a_tokens_default_pair() {
        use crate::pages::{best_pool, own_pool};
        let dir = std::env::temp_dir().join(format!("ps-test-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let (usdc, junk, late) = (STABLES[0], [8u8; 20], [9u8; 20]);
        s.add_token(usdc, meta("USDC", 6), 1);
        s.add_token(junk, meta("BTCB", 18), 1); // seen before LATE, so it quotes LATE/BTCB
        s.add_token(late, meta("LATE", 18), 1);
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
        let (thin, real, bad) = ([2u8; 20], [3u8; 20], [4u8; 20]);
        // BTCB/USDC: $2k of real USDC, one tiny trade prices BTCB at $1e9.
        s.apply(&created(thin, usdc, junk), &store);
        s.apply(&sync(thin, 2_000e6, 2e12), &store);
        s.apply(&swap(thin, 1_000e6, 0.0, 0.0, 1e12), &store);
        // LATE/BTCB with 0.001 BTCB ranks at BTCB's whole $2k, $4k of liquidity, and trades first.
        s.apply(&created(bad, late, junk), &store);
        s.apply(&sync(bad, 1e18, 1e15), &store);
        s.apply(&swap(bad, 0.0, 1e14, 1e18, 0.0), &store);
        let (l, b) = (s.token_ix[&late], s.pool_ix[&pad(&bad)]);
        assert_eq!(best_pool(&s, l), Some(b), "the only pool LATE has");
        // LATE/USDC: $1.5k of USDC, $3k of liquidity, less than the junk pool's $4k.
        s.apply(&created(real, usdc, late), &store);
        s.apply(&sync(real, 1_500e6, 1_500e18), &store);
        let r = s.pool_ix[&pad(&real)];
        assert!((s.liq(b) - 4_000.0).abs() < 1e-6 && (s.liq(r) - 3_000.0).abs() < 1e-6, "{} {}", s.liq(b), s.liq(r));
        // The real pool takes over as the stored best pair at its next trade…
        s.apply(&swap(real, 1e6, 0.0, 0.0, 1e18), &store);
        let checks = |s: &State| {
            assert_eq!(s.tokens[l as usize].best_pool, r);
            assert_eq!((own_pool(s, l), best_pool(s, l)), (Some(r), Some(r)));
        };
        checks(&s);
        // …and keeps it when the junk pool trades again.
        s.apply(&swap(bad, 0.0, 1e14, 1e18, 0.0), &store);
        checks(&s);
        // Under $1k the real pool no longer outranks the junk one.
        s.apply(&sync(real, 400e6, 400e18), &store);
        s.apply(&swap(real, 1e6, 0.0, 0.0, 1e18), &store);
        s.apply(&swap(bad, 0.0, 1e14, 1e18, 0.0), &store);
        assert_eq!(s.tokens[l as usize].best_pool, b);
        assert_eq!((own_pool(&s, l), best_pool(&s, l)), (Some(b), Some(b)));
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
        // Pools in creation order: J/USDC 0, Q/USDC 1, X/J 2, Y/J 3, W/Q 4, V/J 5, J2/Q 6, U/J2 7, Z/USDC 8.
        let (jusdc, qusdc, xj, yj, wq, vj) = ([2u8; 20], [3u8; 20], [4u8; 20], [5u8; 20], [6u8; 20], [7u8; 20]);
        let (j2q, uj2, zusdc) = ([16u8; 20], [17u8; 20], [19u8; 20]);
        let st = &store;
        // J/USDC holds $12.6k of real USDC, J at $0.0126. Q/USDC holds $300k, Q at $1.
        v2_pool(&mut s, st, jusdc, usdc, j, 12_600.0, 1_000_000.0);
        xy(&mut s, st, jusdc, 1.0, 0.0);
        v2_pool(&mut s, st, qusdc, usdc, q, 300_000.0, 300_000.0);
        xy(&mut s, st, qusdc, 1.0, 0.0);
        assert!((s.tokens[1].usd - 0.0126).abs() < 1e-5 && (s.tokens[2].usd - 1.0).abs() < 1e-5);
        // X/J and Y/J: 4M J each, $25.2k of capped liquidity; $12.6k of volume trading flat.
        // W/Q: 40k Q, $80k of liquidity.
        for (pool, quote, base) in [(xj, j, x), (yj, j, y)] {
            v2_pool(&mut s, st, pool, quote, base, 4_000_000.0, 2_000_000.0);
            wash(&mut s, st, pool, 500_000.0);
        }
        v2_pool(&mut s, st, wq, q, w, 40_000.0, 40_000.0);
        xy(&mut s, st, wq, 1.0, 0.0);
        // J's minter overpays in X/J with freshly minted J: X is "up 1e9×" in J and in USD.
        let (r0, r1) = reserves(&s, xj);
        s.apply(&v2_sync(xj, (r0 + 1e15) * 1e18, (r1 - 1.0) * 1e18), st);
        s.apply(&v2_swap(xj, 1e33, 0.0, 0.0, 1e18), st);
        // W really rises 56% against Q.
        xy(&mut s, st, wq, 10_000.0, 0.0);
        xy(&mut s, st, wq, 1.0, 0.0);
        // J really rises 4× as its USDC doubles, but only $25.2k backs it; Y/J trades flat.
        xy(&mut s, st, jusdc, 12_600.0, 0.0);
        xy(&mut s, st, jusdc, 1.0, 0.0);
        assert!((s.tokens[1].usd - 0.0504).abs() < 1e-3, "{}", s.tokens[1].usd);
        wash(&mut s, st, yj, 10.0);
        // V/J opens at J's risen price.
        v2_pool(&mut s, st, vj, j, v, 2_000_000.0, 1_000_000.0);
        wash(&mut s, st, vj, 250_000.0);
        // J2 is priced through the deep Q, but only $12.6k of Q backs it. Its owner burns the pair's
        // J2 and syncs: J2 at $1e9. U/J2 trades flat.
        v2_pool(&mut s, st, j2q, q, j2, 12_600.0, 1_000_000.0);
        xy(&mut s, st, j2q, 1.0, 0.0);
        v2_pool(&mut s, st, uj2, j2, u, 4_000_000.0, 2_000_000.0);
        wash(&mut s, st, uj2, 500_000.0);
        let r0 = reserves(&s, j2q).0;
        s.apply(&v2_sync(j2q, r0 * 1e18, r0 / 1e9 * 1e18), st);
        xy(&mut s, st, j2q, 1.0, 0.0);
        assert!((s.tokens[3].usd / 1e9 - 1.0).abs() < 1e-3, "{}", s.tokens[3].usd);
        wash(&mut s, st, uj2, 10.0);
        // Z/USDC: $40k a side, then Z really falls 50% on $11.7k of selling.
        v2_pool(&mut s, st, zusdc, usdc, z, 40_000.0, 40_000.0);
        xy(&mut s, st, zusdc, 1.0, 0.0);
        xy(&mut s, st, zusdc, 0.0, 16_568.5);
        xy(&mut s, st, zusdc, 0.0, 1.0);
        let list = |s: &State, tab: &str| ranked(s, &Filter { tab: Some(tab.into()), ..Default::default() });
        let ch = |s: &State, p: u32| crate::pages::change(&crate::pages::metrics(s, p), 3);
        let check = |s: &State| {
            for p in [2, 3, 4, 5, 7, 8] {
                let m = crate::pages::metrics(s, p);
                assert!(m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0, "pool {p} passes the list gates");
            }
            assert!(ch(s, 2) > 1e8 && ch(s, 7) > 1e8, "{} {}", ch(s, 2), ch(s, 7));
            // Y/J's rise is real trading, but only $25.2k stands behind J's price.
            assert!(m_thin(s, 3) == 0 && ch(s, 3) > 2.0, "{}", ch(s, 3));
            // J/USDC's own moves were real trading in a stablecoin pair, so it may rank too.
            let gainers: Vec<u32> = list(s, "gainers").into_iter().filter(|p| *p != 0).collect();
            assert_eq!(gainers, vec![4], "only real rises in deep quotes rank");
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
        // J falls back: V/J is down 75%.
        let sold = 1_000_000.0 - reserves(&s, jusdc).1 - 80.0;
        xy(&mut s, st, jusdc, 0.0, sold);
        xy(&mut s, st, jusdc, 0.0, 1.0);
        wash(&mut s, st, vj, 10.0);
        assert!(ch(&s, 5) < -0.7 && m_thin(&s, 5) == 0, "{}", ch(&s, 5));
        check(&s);
        // Stats rebuilt from stored candles rank the same.
        s.flush(st, 12).unwrap();
        let s = State::load(st).unwrap();
        check(&s);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn junk_quote_token_cannot_price_a_small_token() {
        let dir = std::env::temp_dir().join(format!("ps-test-dust-{}", std::process::id()));
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let (usdc, junk, late) = (STABLES[0], [8u8; 20], [9u8; 20]);
        s.add_token(usdc, meta("USDC", 6), 1);
        s.add_token(junk, meta("BTCB", 18), 1); // seen before LATE, so it quotes LATE/BTCB
        s.add_token(late, meta("LATE", 18), 1);
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
        let (thin, real, bad) = ([2u8; 20], [3u8; 20], [4u8; 20]);
        // BTCB/USDC: $2k of real USDC against 0.000002 BTCB, so BTCB is $1e9.
        s.apply(&created(thin, usdc, junk), &store);
        s.apply(&sync(thin, 2_000e6, 2e12), &store);
        s.apply(&swap(thin, 1_000e6, 0.0, 0.0, 1e12), &store);
        assert!((s.tokens[1].usd - 1e9).abs() < 1.0, "{}", s.tokens[1].usd);
        // LATE/USDC: 1.5k LATE against $1.5k. LATE = $1.
        s.apply(&created(real, usdc, late), &store);
        s.apply(&sync(real, 1_500e6, 1_500e18), &store);
        s.apply(&swap(real, 1e6, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[2].usd - 1.0).abs() < 1e-9, "{}", s.tokens[2].usd);
        // LATE/BTCB with 0.001 BTCB counts as BTCB's whole $2k, more than LATE's $1.5k pool,
        // but BTCB is no established quote token, so it weighs $500.
        s.apply(&created(bad, late, junk), &store);
        let (r, b) = (s.pool_ix[&pad(&real)], s.pool_ix[&pad(&bad)]);
        s.apply(&sync(bad, 1e18, 1e15), &store);
        assert!(s.liq(b) > s.liq(r));
        s.apply(&swap(bad, 0.0, 1e14, 1e18, 0.0), &store); // 1 LATE for 0.0001 BTCB = "$100k"
        assert!((s.tokens[2].usd - 1.0).abs() < 1e-9, "junk pool ignored: {}", s.tokens[2].usd);
        assert_eq!(s.tokens[2].ref_pool, r);
        // A price such a pool set before this fix gives way at the real pool's next trade,
        // and the junk pool can't take it back.
        (s.tokens[2].usd, s.tokens[2].ref_pool) = (1e5, b);
        s.apply(&swap(real, 1e6, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[2].usd - 1.0).abs() < 1e-9, "{}", s.tokens[2].usd);
        s.apply(&swap(bad, 0.0, 1e14, 1e18, 0.0), &store);
        assert!((s.tokens[2].usd - 1.0).abs() < 1e-9, "{}", s.tokens[2].usd);

        // An established quote token still prices its pairs: Q at $1 from 1M Q against $1M,
        // and Y/Q holding 3k Q counts in full to beat a $1.2k Y/USDC pool.
        let (q, y, qusdc, yusdc, yq) = ([10u8; 20], [11u8; 20], [5u8; 20], [6u8; 20], [7u8; 20]);
        s.add_token(q, meta("Q", 18), 1);
        s.add_token(y, meta("Y", 18), 1);
        s.apply(&created(qusdc, usdc, q), &store);
        s.apply(&sync(qusdc, 1_000_000e6, 1_000_000e18), &store);
        s.apply(&swap(qusdc, 1e6, 0.0, 0.0, 1e18), &store);
        s.apply(&created(yusdc, usdc, y), &store);
        s.apply(&sync(yusdc, 1_200e6, 600e18), &store);
        s.apply(&swap(yusdc, 2e6, 0.0, 0.0, 1e18), &store); // Y = $2
        assert!((s.tokens[4].usd - 2.0).abs() < 1e-9, "{}", s.tokens[4].usd);
        s.apply(&created(yq, q, y), &store);
        s.apply(&sync(yq, 3_000e18, 1_200e18), &store);
        s.apply(&swap(yq, 2.5e18, 0.0, 0.0, 1e18), &store); // Y = 2.5 Q = $2.50
        assert!((s.tokens[4].usd - 2.5).abs() < 1e-9, "Q pool takes the price: {}", s.tokens[4].usd);
        // Once Q's own pool is drained to $4k a side, Y/Q weighs $750: Y/USDC ($1.2k) takes the
        // price back at its next trade and Y/Q can't retake it.
        s.apply(&sync(qusdc, 4_000e6, 4_000e18), &store);
        s.apply(&swap(yusdc, 2e6, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[4].usd - 2.0).abs() < 1e-9, "{}", s.tokens[4].usd);
        s.apply(&swap(yq, 3e18, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[4].usd - 2.0).abs() < 1e-9, "{}", s.tokens[4].usd);

        // Behind a quote token just under the bar ($99k behind S), Z/S counts for a quarter:
        // 2.5k S can't take Z's price from a $1.2k USDC pool, 5k S can.
        let (sm, z, susdc, zusdc, zs) = ([20u8; 20], [21u8; 20], [12u8; 20], [13u8; 20], [14u8; 20]);
        s.add_token(sm, meta("S", 18), 1);
        s.add_token(z, meta("Z", 18), 1);
        s.apply(&created(susdc, usdc, sm), &store);
        s.apply(&sync(susdc, 99_000e6, 99_000e18), &store);
        s.apply(&swap(susdc, 1e6, 0.0, 0.0, 1e18), &store);
        s.apply(&created(zusdc, usdc, z), &store);
        s.apply(&sync(zusdc, 1_200e6, 600e18), &store);
        s.apply(&swap(zusdc, 2e6, 0.0, 0.0, 1e18), &store);
        s.apply(&created(zs, sm, z), &store);
        s.apply(&sync(zs, 2_500e18, 1_000e18), &store);
        s.apply(&swap(zs, 2.5e18, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[6].usd - 2.0).abs() < 1e-9, "{}", s.tokens[6].usd);
        s.apply(&sync(zs, 5_000e18, 2_000e18), &store);
        s.apply(&swap(zs, 2.5e18, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[6].usd - 2.5).abs() < 1e-9, "{}", s.tokens[6].usd);

        // A token whose only pool is quoted in S is still priced and keeps moving with it…
        let (w, ws, wusdc) = ([22u8; 20], [15u8; 20], [16u8; 20]);
        s.add_token(w, meta("W", 18), 1);
        s.apply(&created(ws, sm, w), &store);
        s.apply(&sync(ws, 30_000e18, 30_000e18), &store);
        s.apply(&swap(ws, 1e18, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[7].usd - 1.0).abs() < 1e-9, "{}", s.tokens[7].usd);
        s.apply(&sync(ws, 30_000e18 * 1.2f64.sqrt(), 30_000e18 / 1.2f64.sqrt()), &store);
        s.apply(&swap(ws, 1.2e18, 0.0, 0.0, 1e18), &store);
        assert!((s.tokens[7].usd - 1.2).abs() < 1e-9, "{}", s.tokens[7].usd);
        // …which a $1.1k USDC pool can't take: W/S still weighs over $8k.
        s.apply(&created(wusdc, usdc, w), &store);
        s.apply(&sync(wusdc, 1_100e6, 0.011e18), &store);
        s.apply(&swap(wusdc, 1_000e6, 0.0, 0.0, 0.01e18), &store); // "$100k"
        assert!((s.tokens[7].usd - 1.2).abs() < 1e-9, "{}", s.tokens[7].usd);

        // A WETH pair counts in full even while WETH's own pool is under the bar: V/WETH with
        // 0.5 WETH ($1.5k) beats a $1.2k V/USDC pool.
        let (v, wethusdc, vusdc, vweth) = ([23u8; 20], [17u8; 20], [18u8; 20], [19u8; 20]);
        s.add_token(WETH, meta("WETH", 18), 1);
        s.add_token(v, meta("V", 18), 1);
        s.apply(&created(wethusdc, WETH, usdc), &store);
        s.apply(&sync(wethusdc, 3e18, 9_000e6), &store);
        s.apply(&swap(wethusdc, 0.0, 3_000e6, 1e18, 0.0), &store); // WETH = $3000
        assert!((s.tokens[8].usd - 3000.0).abs() < 1e-6, "{}", s.tokens[8].usd);
        s.apply(&created(vusdc, usdc, v), &store);
        s.apply(&sync(vusdc, 1_200e6, 600e18), &store);
        s.apply(&swap(vusdc, 2e6, 0.0, 0.0, 1e18), &store);
        s.apply(&created(vweth, WETH, v), &store);
        s.apply(&sync(vweth, 0.5e18, 600e18), &store);
        s.apply(&swap(vweth, 0.001e18, 0.0, 0.0, 1.2e18), &store); // V = $2.50
        assert!((s.tokens[9].usd - 2.5).abs() < 1e-9, "{}", s.tokens[9].usd);
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

    /// A signed 256-bit word (two's complement).
    fn iword(x: f64) -> Vec<u8> {
        if x >= 0.0 { word(x) } else { word(-x - 1.0).iter().map(|b| !b).collect() }
    }

    /// A swap event's tick word at relative √price `sqrt`, as the pool reports it.
    fn tick_at(sqrt: f64) -> Vec<u8> {
        iword((2.0 * sqrt.ln() / 1.0001f64.ln()).floor())
    }

    fn topic(x: f64) -> [u8; 32] {
        iword(x).try_into().unwrap()
    }

    /// Gainers and Losers for a state, and each pair's 24 h change.
    fn lists(s: &State) -> (Vec<u32>, Vec<u32>) {
        use crate::lists::{Filter, ranked};
        let list = |tab: &str| ranked(s, &Filter { tab: Some(tab.into()), ..Default::default() });
        (list("gainers"), list("losers"))
    }

    fn change(s: &State, p: u32) -> f64 {
        crate::pages::change(&crate::pages::metrics(s, p), 3)
    }

    #[test]
    fn v2_pool_owner_cannot_fake_a_move() {
        let dir = std::env::temp_dir().join(format!("ps-test-v2-fake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let usdc = STABLES[0];
        s.add_token(usdc, meta("USDC", 6), 1);
        let (w, l, j, z, d) = ([10u8; 20], [11u8; 20], [12u8; 20], [13u8; 20], [14u8; 20]);
        for t in [w, l, j, z, d] {
            s.add_token(t, meta("T", 18), 1);
        }
        let uniswap = hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6");
        let sync = |pool: Addr, r0: f64, r1: f64| log(11, pool, vec![T.v2_sync], [word(r0), word(r1)].concat());
        let swap = |pool: Addr, a0in: f64, a1in: f64, a0out: f64, a1out: f64| {
            let data = [word(a0in), word(a1in), word(a0out), word(a1out)].concat();
            log(11, pool, vec![T.v2_swap, [0; 32], [0; 32]], data)
        };
        let reserves = |s: &State, pool: Addr| {
            let p = &s.pools[s.pool_ix[&pad(&pool)] as usize];
            (p.bal0, p.bal1)
        };
        // x·y trades in whole tokens, Sync first as on chain. USDC is token0 everywhere.
        let buy = |s: &mut State, pool: Addr, q: f64| {
            let (r0, r1) = reserves(s, pool);
            let out = r1 * q / (r0 + q);
            s.apply(&sync(pool, (r0 + q) * 1e6, (r1 - out) * 1e18), &store);
            s.apply(&swap(pool, q * 1e6, 0.0, 0.0, out * 1e18), &store);
        };
        let sell = |s: &mut State, pool: Addr, b: f64| {
            let (r0, r1) = reserves(s, pool);
            let out = r0 * b / (r1 + b);
            s.apply(&sync(pool, (r0 - out) * 1e6, (r1 + b) * 1e18), &store);
            s.apply(&swap(pool, 0.0, b * 1e18, out * 1e6, 0.0), &store);
        };
        // Pools 0..5: W, L, J, Z and D against USDC, each opened with a $1 trade.
        let pools = [[2u8; 20], [3u8; 20], [4u8; 20], [5u8; 20], [6u8; 20]];
        let (wp, lp, jp, zp, dp) = (pools[0], pools[1], pools[2], pools[3], pools[4]);
        for ((pool, t), (r0, r1)) in pools.iter().zip([w, l, j, z, d]).zip([
            (100_000.0, 100_000.0),
            (100_000.0, 100_000.0),
            (20_000.0, 1_600_000.0),
            (1_000.0, 1_000_000.0),
            (40_000.0, 40_000.0),
        ]) {
            let mut data = pad(pool).to_vec();
            data.extend(word(1.0));
            s.apply(&log(10, uniswap, vec![T.v2_pair_created, pad(&usdc), pad(&t)], data), &store);
            s.apply(&sync(*pool, r0 * 1e6, r1 * 1e18), &store);
            buy(&mut s, *pool, 1.0);
        }
        // W's spot price really rises 50% and L's really falls 50%, through $100k of depth.
        buy(&mut s, wp, 22_474.487);
        sell(&mut s, lp, 41_421.356);
        // J's owner wash-trades $10k, then pays 1,000 USDC for 0.000001 J: "J at $1e9".
        let r1 = reserves(&s, jp).1;
        buy(&mut s, jp, 5_000.0);
        let bought = r1 - reserves(&s, jp).1;
        sell(&mut s, jp, bought);
        let (r0, r1) = reserves(&s, jp);
        s.apply(&sync(jp, (r0 + 1_000.0) * 1e6, (r1 - 1e-6) * 1e18), &store);
        s.apply(&swap(jp, 1_000e6, 0.0, 0.0, 1e12), &store);
        // Z's owner pumps it 100x while the pool holds $1k, then adds $40k at the new price and
        // wash-trades $10k there.
        buy(&mut s, zp, 9_000.0);
        let (r0, r1) = reserves(&s, zp);
        s.apply(&sync(zp, r0 * 4e6, r1 * 4e18), &store);
        buy(&mut s, zp, 5_000.0);
        let bought = reserves(&s, zp).1 * 5_000.0 / 45_000.0;
        sell(&mut s, zp, bought);
        // D's owner pulls the liquidity down to $1k, dumps D 99%, then puts it back.
        let (r0, r1) = reserves(&s, dp);
        s.apply(&sync(dp, r0 / 40.0 * 1e6, r1 / 40.0 * 1e18), &store);
        sell(&mut s, dp, 9_000.0);
        let (r0, r1) = reserves(&s, dp);
        s.apply(&sync(dp, r0 * 400.0 * 1e6, r1 * 400.0 * 1e18), &store);
        buy(&mut s, dp, 5_000.0);
        let bought = reserves(&s, dp).1 * 5_000.0 / 45_000.0;
        sell(&mut s, dp, bought);

        let check = |s: &State| {
            for p in 0..5 {
                let m = crate::pages::metrics(s, p);
                assert!(m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0, "pool {p} passes the list gates");
            }
            // Each trade's average price: √1.5 and 1/√2 of where they started.
            assert!((change(s, 0) - 0.2247).abs() < 1e-3 && (change(s, 1) + 0.2929).abs() < 1e-3);
            assert!(change(s, 2) < 0.11, "the overpay moves J no further than the reserves: {}", change(s, 2));
            assert!(change(s, 3) > 50.0 && change(s, 4) < -0.95, "{} {}", change(s, 3), change(s, 4));
            assert_eq!(lists(s), (vec![0, 2], vec![1]), "Z's and D's thin moves don't rank");
        };
        check(&s);
        // The overpaying trade itself still shows what was paid.
        s.flush(&store, 12).unwrap();
        let t = &store.trades(2, (u32::MAX, 0), 1)[0].1;
        assert!((t.price_usd - 1e9).abs() < 1e3, "{}", t.price_usd);
        // Stats rebuilt from stored candles rank the same.
        check(&State::load(&store).unwrap());
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cl_depth_is_measured_at_the_price() {
        let dir = std::env::temp_dir().join(format!("ps-test-cl-depth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        let mut s = State::default();
        let usdc = STABLES[0];
        s.add_token(usdc, meta("USDC", 6), 1);
        let (w, l, j) = ([10u8; 20], [11u8; 20], [12u8; 20]);
        for t in [w, l, j] {
            s.add_token(t, meta("T", 18), 1);
        }
        let v3 = hex20("33128a8fc17869897dce68ed026d694621f6fdfd");
        let created = |pool: Addr, t0: Addr, t1: Addr| {
            let topics = vec![T.v3_pool_created, pad(&t0), pad(&t1), topic(500.0)];
            log(10, v3, topics, [word(10.0), pad(&pool).to_vec()].concat())
        };
        let mint = |pool: Addr, lo: f64, hi: f64, l: f64, a0: f64, a1: f64| {
            let topics = vec![T.v3_mint, [0; 32], topic(lo), topic(hi)];
            log(11, pool, topics, [word(0.0), word(l), word(a0), word(a1)].concat())
        };
        let burn = |pool: Addr, lo: f64, hi: f64, l: f64| {
            let topics = vec![T.v3_burn, [0; 32], topic(lo), topic(hi)];
            log(11, pool, topics, [word(l), word(0.0), word(0.0)].concat())
        };
        // Raw amounts the pool gains, √price as a multiple of 2^96, in-range liquidity after.
        let swap = |pool: Addr, a0: f64, a1: f64, sqrt: f64, l: f64| {
            let data = [iword(a0), iword(a1), word(sqrt * Q96), word(l), tick_at(sqrt)].concat();
            log(11, pool, vec![T.v3_swap, [0; 32], [0; 32]], data)
        };
        let full = (-887_220.0, 887_220.0);
        let (wp, lp, jp) = ([2u8; 20], [3u8; 20], [4u8; 20]);
        // W/USDC (USDC is token0) and L/USDC (USDC is token1): $100k a side over the full range,
        // L = 1e17, both tokens at $1.
        s.apply(&created(wp, usdc, w), &store);
        s.apply(&mint(wp, full.0, full.1, 1e17, 100_000e6, 100_000e18), &store);
        s.apply(&swap(wp, 1e6, -1e18, 1e6, 1e17), &store);
        s.apply(&created(lp, l, usdc), &store);
        s.apply(&mint(lp, full.0, full.1, 1e17, 100_000e18, 100_000e6), &store);
        s.apply(&swap(lp, -1e18, 1e6, 1e-6, 1e17), &store);
        assert!(
            (s.liq(0) - 200_000.0).abs() < 10.0 && (s.liq(1) - 200_000.0).abs() < 10.0,
            "{} {}",
            s.liq(0),
            s.liq(1)
        );
        // W's spot price rises 50% and L's falls 50%, through that depth.
        s.apply(&swap(wp, 22_474.487e6, -18_350.342e18, 1e6 / 1.5f64.sqrt(), 1e17), &store);
        s.apply(&swap(lp, 41_421.356e18, -29_289.322e6, 1e-6 * 0.5f64.sqrt(), 1e17), &store);
        // J/USDC: $12.6k of USDC parked in a range far above the price, which passed the $25k
        // liquidity floor, and dust liquidity (L = 1e9) around J at $1.
        s.apply(&created(jp, usdc, j), &store);
        s.apply(&mint(jp, 600_000.0, 700_000.0, 1e15, 12_600e6, 0.0), &store);
        s.apply(&mint(jp, 0.0, 887_220.0, 1e9, 1.0, 1e6), &store);
        s.apply(&swap(jp, 1e6, -1e18, 1e6, 1e9), &store);
        assert!(s.liq(2) < 10.0, "parked USDC isn't depth: {}", s.liq(2));
        // The owner pumps J a million times through the dust, then adds $20k of real depth at
        // the new price (L = 2e13) and wash-trades $10k there.
        s.apply(&swap(jp, 1e6, -1e12, 1e3, 1e9), &store);
        s.apply(&mint(jp, 100_000.0, 200_000.0, 2e13, 20_000e6, 20e18), &store);
        assert!((s.liq(2) - 40_000.0).abs() < 10.0, "{}", s.liq(2));
        s.apply(&swap(jp, 5_000e6, -5e15, 1e3, 2e13 + 1e9), &store);
        s.apply(&swap(jp, -5_000e6, 5e15, 1e3, 2e13 + 1e9), &store);
        let check = |s: &State| {
            for p in 0..3 {
                let m = crate::pages::metrics(s, p);
                assert!(m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0, "pool {p} passes the list gates");
            }
            assert!((change(s, 0) - 0.2247).abs() < 1e-3 && (change(s, 1) + 0.2929).abs() < 1e-3);
            assert!(change(s, 2) > 1e5, "{}", change(s, 2));
            assert_eq!(lists(s), (vec![0], vec![1]), "J's pump through dust doesn't rank");
        };
        check(&s);
        s.flush(&store, 12).unwrap();
        check(&State::load(&store).unwrap());
        // Burning that depth (v3 Burn moves no tokens until Collect) takes it off at once.
        s.apply(&burn(jp, 100_000.0, 200_000.0, 2e13), &store);
        assert!(s.liq(2) < 10.0, "{}", s.liq(2));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Whether pool `p` would pass the Gainers/Losers gates on liquidity and volume, and whether
    /// it ranks on either list in any window.
    fn gated_and_ranked(s: &State, p: u32) -> (bool, bool) {
        use crate::lists::{Filter, ranked};
        let m = crate::pages::metrics(s, p);
        let ranks = ["5m", "1h", "6h", "24h"].iter().any(|w| {
            ["gainers", "losers"].iter().any(|tab| {
                let f = Filter { tab: Some(tab.to_string()), w: Some(w.to_string()), ..Default::default() };
                ranked(s, &f).contains(&p)
            })
        });
        (m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0, ranks)
    }

    fn fresh(name: &str) -> (Store, State, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ps-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = State::default();
        s.add_token(STABLES[0], meta("USDC", 6), 1);
        for t in [[10u8; 20], [11u8; 20], [12u8; 20]] {
            s.add_token(t, meta("T", 18), 1);
        }
        (Store::open(dir.to_str().unwrap()).unwrap(), s, dir)
    }

    /// v3 logs for pool `jp` (USDC/J, USDC token0) at `block`.
    fn v3_log(block: u32, jp: Addr, topic0: [u8; 32], ticks: Option<(f64, f64)>, data: Vec<Vec<u8>>) -> RawLog {
        let topics = match ticks {
            Some((lo, hi)) => vec![topic0, [0; 32], topic(lo), topic(hi)],
            None => vec![topic0, [0; 32], [0; 32], [0; 32]],
        };
        log(block, jp, topics, data.concat())
    }

    fn v3_swap(block: u32, jp: Addr, a0: f64, a1: f64, sqrt: f64, l: f64) -> RawLog {
        log(
            block,
            jp,
            vec![T.v3_swap, [0; 32], [0; 32]],
            [iword(a0), iword(a1), word(sqrt * Q96), word(l), tick_at(sqrt)].concat(),
        )
    }

    /// J/USDC v3 with $100k a side over the full range (L = 1e17), opened at J = $1.
    fn v3_pool(s: &mut State, store: &Store) -> Addr {
        let (usdc, j, jp) = (STABLES[0], [12u8; 20], [4u8; 20]);
        let v3 = hex20("33128a8fc17869897dce68ed026d694621f6fdfd");
        let topics = vec![T.v3_pool_created, pad(&usdc), pad(&j), topic(500.0)];
        s.apply(&log(10, v3, topics, [word(10.0), pad(&jp).to_vec()].concat()), store);
        let full = Some((-887_220.0, 887_220.0));
        s.apply(
            &v3_log(11, jp, T.v3_mint, full, vec![word(0.0), word(1e17), word(100_000e6), word(100_000e18)]),
            store,
        );
        s.apply(&v3_swap(11, jp, 1_000e6, -1_000e18, 1e6, 1e17), store);
        jp
    }

    /// `v3_pool`, then the owner burns and collects all but `keep` liquidity.
    fn v3_drained(s: &mut State, store: &Store, keep: f64) -> Addr {
        let jp = v3_pool(s, store);
        let full = Some((-887_220.0, 887_220.0));
        s.apply(&v3_log(12, jp, T.v3_burn, full, vec![word(1e17 - keep), word(101_000e6), word(99_000e18)]), store);
        s.apply(&v3_log(12, jp, T.v3_collect, None, vec![word(0.0), word(101_000e6), word(99_000e18)]), store);
        jp
    }

    #[test]
    fn cl_swap_through_an_empty_range_counts_as_unbacked() {
        for old in [false, true] {
            let (store, mut s, dir) = fresh(if old { "cl-empty-old" } else { "cl-empty" });
            let jp = v3_drained(&mut s, &store, 0.0);
            if old {
                // A record stored before we kept `spot`, liquidity and tick.
                let b = s.pools[0].encode();
                s.pools[0] = Pool::decode(&b[..b.len() - 29]);
                (s.pools[0].liquidity, s.pools[0].tick) = (f64::NAN, f64::NAN);
                assert!(s.pools[0].spot.is_nan());
            }
            // A swap through the empty range exchanges nothing but moves J to $1e6. The owner
            // adds $20k at the new price and wash-trades $10k there.
            s.apply(&v3_swap(12, jp, 0.0, 0.0, 1e3, 0.0), &store);
            s.apply(
                &v3_log(
                    13,
                    jp,
                    T.v3_mint,
                    Some((100_000.0, 200_000.0)),
                    vec![word(0.0), word(2e13), word(20_000e6), word(20e18)],
                ),
                &store,
            );
            s.apply(&v3_swap(14, jp, 5_000e6, -5e15, 1e3, 2e13), &store);
            s.apply(&v3_swap(15, jp, -5_000e6, 5e15, 1e3, 2e13), &store);
            assert!(change(&s, 0) > 1e5, "old {old}: {}", change(&s, 0));
            assert_eq!(gated_and_ranked(&s, 0), (true, false), "old {old}");
            drop(store);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn cl_thin_pump_counts_after_its_window_rolls() {
        let (store, mut s, dir) = fresh("cl-roll");
        let jp = v3_drained(&mut s, &store, 1e9);
        // 1 USDC for 0.001 J through dust: spot to $1e6, the trade itself at $1000.
        s.apply(&v3_swap(13, jp, 1e6, -1e15, 1e3, 1e9), &store);
        // Six minutes later: $20k of depth at the new price and a $10k wash.
        s.apply(
            &v3_log(
                400,
                jp,
                T.v3_mint,
                Some((100_000.0, 200_000.0)),
                vec![word(0.0), word(2e13), word(20_000e6), word(20e18)],
            ),
            &store,
        );
        s.apply(&v3_swap(401, jp, 5_000e6, -5e15, 1e3, 2e13 + 1e9), &store);
        s.apply(&v3_swap(402, jp, -5_000e6, 5e15, 1e3, 2e13 + 1e9), &store);
        let m = crate::pages::metrics(&s, 0);
        assert!(crate::pages::change(&m, 0) > 100.0, "the 5m window opens at the thin trade's price");
        assert_eq!(gated_and_ranked(&s, 0), (true, false));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v2_move_by_sync_alone_counts_as_unbacked() {
        let (store, mut s, dir) = fresh("v2-sync");
        let (usdc, j, jp) = (STABLES[0], [12u8; 20], [4u8; 20]);
        v2_pool(&mut s, &store, jp, usdc, j, 13_000.0, 13_000.0);
        xy(&mut s, &store, jp, 1.0, 0.0);
        // J's owner burns 99% of the pair's J (a backdoor, or a rebase) and calls sync(), on a
        // pool stored before we kept its spot price.
        s.pools[0].spot = f64::NAN;
        let (r0, r1) = reserves(&s, jp);
        s.apply(&v2_sync(jp, r0 * 1e6, r1 / 100.0 * 1e18), &store);
        wash(&mut s, &store, jp, 5_000.0);
        assert!(change(&s, 0) > 50.0, "{}", change(&s, 0));
        assert_eq!(gated_and_ranked(&s, 0), (true, false));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v2_overpay_into_a_shallow_pool_counts_as_unbacked() {
        let (store, mut s, dir) = fresh("v2-shallow");
        let (usdc, j, jp) = (STABLES[0], [12u8; 20], [4u8; 20]);
        v2_pool(&mut s, &store, jp, usdc, j, 20_000.0, 20_000.0);
        xy(&mut s, &store, jp, 1.0, 0.0);
        // The owner withdraws down to $4.85k a side (the price doesn't move).
        let (r0, r1) = reserves(&s, jp);
        s.apply(&v2_sync(jp, r0 * 0.2425 * 1e6, r1 * 0.2425 * 1e18), &store);
        // Paying 7,760 USDC for a speck of J moves the reserves 2.6× while the pool held $4.85k:
        // the surplus reads as depth over that move, but no more than $4.85k was ever there.
        let (r0, r1) = reserves(&s, jp);
        s.apply(&v2_sync(jp, (r0 + 7_760.0) * 1e6, (r1 - 1e-6) * 1e18), &store);
        s.apply(&v2_swap(jp, 7_760e6, 0.0, 0.0, 1e12), &store);
        wash(&mut s, &store, jp, 5_000.0);
        assert!(change(&s, 0) > 1.0, "{}", change(&s, 0));
        assert_eq!(gated_and_ranked(&s, 0), (true, false));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v4_fee_inflated_price_moves_no_further_than_the_curve() {
        let (store, mut s, dir) = fresh("v4-fee");
        let (usdc, j, id) = (STABLES[0], [12u8; 20], [5u8; 32]);
        let pm = |block: u32, topic: [u8; 32], data: Vec<Vec<u8>>| {
            log(block, abi::V4_POOL_MANAGER, vec![topic, id, pad(&usdc), pad(&j)], data.concat())
        };
        // $13k of USDC in range at J = $1 (L = 1.3e16).
        let init = vec![word(3000.0), word(60.0), word(0.0), word(1e6 * Q96), word(276_324.0)];
        s.apply(&pm(10, T.v4_initialize, init), &store);
        s.apply(&pm(10, T.v4_modify, vec![iword(-887_220.0), word(887_220.0), word(1.3e16), word(0.0)]), &store);
        // Swapper's deltas: USDC paid in is negative.
        let sw = |b: u32, usdc_in: f64, j_out: f64, sqrt: f64, fee: f64| {
            pm(
                b,
                T.v4_swap,
                vec![iword(-usdc_in), iword(j_out), word(sqrt * Q96), word(1.3e16), tick_at(sqrt), word(fee)],
            )
        };
        s.apply(&sw(11, 1e6, 1e18, 1e6, 3000.0), &store);
        s.apply(&sw(12, 5_000e6, 5_000e18, 1e6, 3000.0), &store);
        s.apply(&sw(13, -5_000e6, -5_000e18, 1e6, 3000.0), &store);
        assert_eq!(s.stats[&0].window(s.head_ts, 86_400).thin, 0, "a 0.3% fee is no move");
        // The hook charges 99%, paid to the LP: 1,000 USDC for 10 J barely moves the pool.
        s.apply(&sw(14, 1_000e6, 10e18, 1e6 * (1.0 - 5e-4), 990_000.0), &store);
        assert!(change(&s, 0) < 0.06, "no further than fee slack: {}", change(&s, 0));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn thin_move_of_the_quote_token_counts_for_its_pairs() {
        // Block b is at 300 + b seconds past an hour.
        // 0: X's pricing pool is pumped; 1: then a fresh, deeper pool takes over X's price
        // cleanly; 2: a side pool is pumped (it can't set X's price), then deepened to take over.
        for case in 0..3 {
            let (store, mut s, dir) = fresh(&format!("hop{case}"));
            let (usdc, x, j) = (STABLES[0], [10u8; 20], [12u8; 20]);
            let (xp, jxp, xp2) = ([2u8; 20], [3u8; 20], [5u8; 20]);
            let st = &store;
            // X/USDC with $200k a side; J/X with 20k a side. Both open at 1.
            v2_pool(&mut s, st, xp, usdc, x, 200_000.0, 200_000.0);
            xy(&mut s, st, xp, 1.0, 0.0);
            v2_pool(&mut s, st, jxp, x, j, 20_000.0, 20_000.0);
            xy(&mut s, st, jxp, 1.0, 0.0);
            // X really rises 21% and J/X wash-trades: J is up 21% in USD, which counts.
            xy_at(&mut s, st, xp, 20_000.0, 0.0, 12);
            xy_at(&mut s, st, xp, 1.0, 0.0, 12);
            wash_at(&mut s, st, jxp, 6_000.0, 13);
            assert_eq!(m_thin(&s, 1), 0);
            assert!(change(&s, 1) > 0.2, "{}", change(&s, 1));
            // 9 s later X's owner pulls an X/USDC pool down to $1.2k, pumps X 100× there (thin)
            // and puts $480k back (in case 2 a side pool, deepened to outweigh X/USDC).
            let pumped = if case == 2 { xp2 } else { xp };
            if case == 2 {
                let (r0, r1) = reserves(&s, xp);
                v2_pool(&mut s, st, xp2, usdc, x, r0 / 200.0, r1 / 200.0);
            } else {
                let (r0, r1) = reserves(&s, xp);
                s.apply(&at(v2_sync(xp, r0 / 200.0 * 1e6, r1 / 200.0 * 1e18), 22), st);
            }
            xy_at(&mut s, st, pumped, 10_890.0, 0.0, 22);
            xy_at(&mut s, st, pumped, 0.001, 0.0, 23);
            let (r0, r1) = reserves(&s, pumped);
            let k = if case == 2 { 80.0 } else { 40.0 };
            s.apply(&at(v2_sync(pumped, r0 * k * 1e6, r1 * k * 1e18), 23), st);
            if case == 2 {
                xy_at(&mut s, st, xp2, 1.0, 0.0, 30);
                assert_eq!(s.tokens[1].ref_pool, s.pool_ix[&pad(&xp2)]);
            } else if case == 1 {
                // A fresh, deeper X/USDC pool at the new price takes over X's price cleanly.
                let (r0, r1) = reserves(&s, xp);
                v2_pool(&mut s, st, xp2, usdc, x, r0 * 2.0, r1 * 2.0);
                xy_at(&mut s, st, xp2, 1.0, 0.0, 30);
                assert_eq!(s.tokens[1].ref_pool, s.pool_ix[&pad(&xp2)]);
            } else {
                xy_at(&mut s, st, xp, 1.0, 0.0, 30);
            }
            assert!(s.deep_quote(1), "X passes the deep-quote gate");
            // Two hours later, 5 s past the hour, J/X wash-trades flat in X: J "up 100×" in USD.
            wash_at(&mut s, st, jxp, 6_000.0, 3_300 + 7_200 + 5);
            assert!(change(&s, 1) > 50.0, "{}", change(&s, 1));
            assert_eq!(gated_and_ranked(&s, 1), (true, false), "case {case}");
            drop(store);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn dust_swap_in_a_cl_pool_is_no_move() {
        let (store, mut s, dir) = fresh("cl-dust");
        let jp = v3_pool(&mut s, &store);
        // 2 wei of USDC for 0.000001 J (rounding: "J at $2"), then a normal $100 trade.
        s.apply(&v3_swap(13, jp, 2.0, -1e12, 1e6, 1e17), &store);
        s.apply(&v3_swap(14, jp, 100e6, -99.9e18, 1e6 * (1.0 - 5e-7), 1e17), &store);
        assert_eq!(m_thin(&s, 0), 0);
        assert!(change(&s, 0).abs() < 0.06, "{}", change(&s, 0));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spot_clamp_follows_the_curve() {
        // x·y: a trade's price stays between the spot prices before and after it.
        assert_eq!(clamp_spot(1e9, (1.0, 1.1), false), 1.1 * SPOT_SLACK);
        assert_eq!(clamp_spot(1.05, (1.0, 1.1), false), 1.05);
        // A stable pool holding 2:1 trades near 1.08 (14/13), not at the reserve ratio.
        assert_eq!(clamp_spot(1.076, (2.0, 1.98), true), 1.076);
        assert!(clamp_spot(1.076, (2.0, 1.98), false) > 1.8);
        // An empty pool before the trade bounds nothing from below.
        assert_eq!(clamp_spot(0.5, (0.0, 1.0), false), 0.5);
        // Unknown spot prices leave the price alone.
        assert_eq!(clamp_spot(7.0, (f64::NAN, f64::NAN), false), 7.0);
    }

    #[test]
    fn old_records_mean_unknown() {
        let mut s = State::default();
        s.add_token([1; 20], meta("A", 18), 1);
        s.add_token([2; 20], meta("B", 18), 1);
        s.create([3; 32], Kind::Solidly, [1; 20], [2; 20], 0, (0.0, 0), [0; 20], true, 1, 1);
        s.pools[0].tick = -7.0;
        let p = &s.pools[0];
        assert_eq!(Pool::decode(&p.encode()).stable, Some(true));
        assert_eq!(Pool::decode(&p.encode()).tick, -7.0);
        // A record stored before these fields: liquidity, curve and tick unknown.
        let old = &p.encode()[..p.encode().len() - 29];
        let d = Pool::decode(old);
        assert!(d.liquidity.is_nan() && d.stable.is_none() && d.tick.is_nan());
        let c = Candle { thin: 2, ..Default::default() };
        assert_eq!(Candle::decode(&c.encode()).thin, 2);
        assert_eq!(Candle::decode(&c.encode()[..44]).thin, 0);
    }

    #[test]
    fn v4_fee_is_not_depth() {
        // A hookless v4 pool may charge up to 100%, paid to its LP, who is the attacker here:
        // 120k USDC (flash-borrowed) moves J 100× through dust, and $1.80 of it reaches the
        // curve. Counting the whole 120k as depth made the move look backed.
        let (store, mut s, dir) = fresh("v4-fee-depth");
        let (usdc, j, id) = (STABLES[0], [12u8; 20], [5u8; 32]);
        let pm = |block: u32, topic: [u8; 32], data: Vec<Vec<u8>>| {
            log(block, abi::V4_POOL_MANAGER, vec![topic, id, pad(&usdc), pad(&j)], data.concat())
        };
        // USDC/J at $1, full-range dust liquidity (about $0.2 a side), no hooks, 99.9985% fee.
        let fee = 999_985.0;
        let init = vec![word(fee), word(1.0), word(0.0), word(1e6 * Q96), word(276_324.0)];
        s.apply(&pm(10, T.v4_initialize, init), &store);
        s.apply(&pm(10, T.v4_modify, vec![iword(-887_220.0), word(887_220.0), word(2e11), word(0.0)]), &store);
        let sw = |b: u32, usdc_in: f64, j_out: f64, sqrt: f64| {
            let data = vec![iword(-usdc_in), iword(j_out), word(sqrt * Q96), word(2e11), tick_at(sqrt), word(fee)];
            pm(b, T.v4_swap, data)
        };
        s.apply(&sw(10, 0.01e6, 1e10, 1e6 * (1.0 - 1e-9)), &store);
        let b = 10 + 90_000;
        s.apply(&sw(b, 120_000e6, 1.8e17, 1e5), &store);
        // Real depth arrives at the new price afterwards: ±10k ticks, L = 6e15.
        let tick = (2.0 * (1e5f64).ln() / 1.0001f64.ln()).floor();
        s.apply(
            &pm(b + 1, T.v4_modify, vec![iword(tick - 10_000.0), iword(tick + 10_000.0), word(6e15), word(0.0)]),
            &store,
        );
        let m = crate::pages::metrics(&s, 0);
        assert!(m.liq >= 25_000.0 && m.w[3].vol >= 10_000.0 && change(&s, 0) > 50.0);
        assert!(m.w[3].thin > 0);
        assert_eq!(gated_and_ranked(&s, 0), (true, false));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn small_steps_through_dust_into_one_tick_positions_are_thin() {
        // Each step parks a big one-tick position just past the price (holding only J), swaps
        // 1 wei through the dust into it, then burns it: under 2 ticks a step, a move too small
        // to measure, where the liquidity after the swap is the parked position's. `at_boundary`
        // lands every step exactly on a tick boundary, where the pool's tick (one below, coming
        // from above) isn't the one √price rounds to: positions just above the price would count
        // as in range.
        for at_boundary in [false, true] {
            let (store, mut s, dir) = fresh(&format!("tick-walk-{at_boundary}"));
            let jp = v3_drained(&mut s, &store, 1e9);
            // 13k USDC parked far above the price (USDC only).
            let park = vec![word(0.0), word(1e15), word(13_000e6), word(0.0)];
            s.apply(&v3_log(12, jp, T.v3_mint, Some((600_000.0, 700_000.0)), park), &store);
            let big = 1e17;
            let mint = |lo: f64, l: f64| {
                let topic = if l > 0.0 { T.v3_mint } else { T.v3_burn };
                let data = if l > 0.0 {
                    vec![word(0.0), word(l), word(0.0), word(1e15)]
                } else {
                    vec![word(-l), word(0.0), word(0.0)]
                };
                v3_log(13, jp, topic, Some((lo, lo + 1.0)), data)
            };
            let mut x = (2.0 * (1e6f64).ln() / 1.0001f64.ln()).floor();
            for _ in 0..7_400 {
                if at_boundary {
                    // At boundary x, pool tick x - 1. A position at [x, x + 1) is out of range.
                    x -= 1.0;
                    s.apply(&mint(x + 1.0, big), &store);
                    s.apply(&mint(x - 1.0, big), &store);
                    // √price as TickMath gives it, rounded up; the pool's tick is one below.
                    let sqrt = 1.0001f64.powf(x / 2.0) * (1.0 + 1e-12);
                    let data = [iword(1.0), iword(-1e9), word(sqrt * Q96), word(big + 1e9), iword(x - 1.0)];
                    s.apply(&log(13, jp, vec![T.v3_swap, [0; 32], [0; 32]], data.concat()), &store);
                    s.apply(&mint(x + 1.0, -big), &store);
                    s.apply(&mint(x - 1.0, -big), &store);
                } else {
                    x -= 1.5;
                    let lo = x.floor();
                    s.apply(&mint(lo, big), &store);
                    s.apply(&v3_swap(13, jp, 1.0, -1e9, 1.0001f64.powf(x / 2.0), big + 1e9), &store);
                    s.apply(&mint(lo, -big), &store);
                }
            }
            assert!(s.pools[0].liquidity < 1e10, "{}", s.pools[0].liquidity);
            // $20k of real depth at the new price and a $10k wash.
            let sq = 1.0001f64.powf(x / 2.0);
            let lo = (x - 5000.0).floor();
            let depth = vec![word(0.0), word(2e16), word(20_000e6), word(20e18)];
            s.apply(&v3_log(14, jp, T.v3_mint, Some((lo, lo + 10_000.0)), depth), &store);
            s.apply(&v3_swap(15, jp, 5_000e6, -2e21, sq * (1.0 - 1e-4), 2e16 + 1e9), &store);
            s.apply(&v3_swap(16, jp, -5_000e6, 2e21, sq, 2e16 + 1e9), &store);
            assert!(change(&s, 0) > 0.5, "{}", change(&s, 0));
            assert!(m_thin(&s, 0) > 0);
            assert_eq!(gated_and_ranked(&s, 0), (true, false));
            drop(store);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn deep_pool_crossing_into_more_liquidity_is_not_thin() {
        // A $200k full-range USDC/J pool (L = 1e17); an LP adds L = 1e18 just below the price.
        // A $33 swap moves 1.5 ticks into it: the depth is the pool's, not a tenth of it.
        let (store, mut s, dir) = fresh("deep-cross");
        let jp = v3_pool(&mut s, &store);
        let t = (2.0 * (1e6f64).ln() / 1.0001f64.ln()).floor();
        let lp = vec![word(0.0), word(1e18), word(0.0), word(1.2e21)];
        s.apply(&v3_log(12, jp, T.v3_mint, Some((t - 24.0, t - 1.0)), lp), &store);
        assert!((s.pools[0].liquidity - 1e17).abs() < 1.0);
        s.apply(&v3_swap(13, jp, 33e6, -33e18, 1e6 * 1.0001f64.powf(-0.75), 1.1e18), &store);
        assert_eq!(m_thin(&s, 0), 0);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quote_pump_split_into_small_steps_counts_for_its_pairs() {
        // As `thin_move_of_the_quote_token_counts_for_its_pairs`, case 0, in 30 thin steps of
        // about 4.9% each, with J/X wash-traded after each.
        let (store, mut s, dir) = fresh("quote-steps");
        let (usdc, x, j) = (STABLES[0], [10u8; 20], [12u8; 20]);
        let (xp, jxp) = ([2u8; 20], [3u8; 20]);
        let st = &store;
        v2_pool(&mut s, st, xp, usdc, x, 200_000.0, 200_000.0);
        xy(&mut s, st, xp, 1.0, 0.0);
        v2_pool(&mut s, st, jxp, x, j, 20_000.0, 20_000.0);
        xy(&mut s, st, jxp, 1.0, 0.0);
        let mut b = 20;
        for _ in 0..30 {
            let (r0, r1) = reserves(&s, xp);
            s.apply(&at(v2_sync(xp, r0 / 200.0 * 1e6, r1 / 200.0 * 1e18), b), st);
            let q = reserves(&s, xp).0;
            xy_at(&mut s, st, xp, q * 0.0238, 0.0, b);
            let (r0, r1) = reserves(&s, xp);
            s.apply(&at(v2_sync(xp, r0 * 200.0 * 1e6, r1 * 200.0 * 1e18), b), st);
            wash_at(&mut s, st, jxp, 300.0, b + 1);
            b += 10;
        }
        assert!(s.tokens[1].usd > 3.0 && s.deep_quote(1));
        assert!(change(&s, 1) > 2.0 && m_thin(&s, 1) > 0);
        assert_eq!(gated_and_ranked(&s, 1), (true, false));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn solidly_pool_of_unknown_curve_prices_as_x_y() {
        // A volatile USDC/J pool stored before we kept the curve (`stable: None`), $13k a side:
        // overpaying 30k USDC for a speck of J may record the spot price it leaves, not the
        // stable curve's (about 3× higher here).
        let (store, mut s, dir) = fresh("solidly-unknown");
        let (usdc, j, jp) = (STABLES[0], [12u8; 20], [4u8; 20]);
        let aero = hex20("420dd381b31aef6683db6b902084cb0ffece40da");
        let data = [pad(&jp).to_vec(), word(1.0)].concat();
        s.apply(&log(10, aero, vec![T.solidly_pool_created, pad(&usdc), pad(&j), [0; 32]], data), &store);
        s.pools[0].stable = None;
        let sync = |r0: f64, r1: f64| log(11, jp, vec![T.solidly_sync], [word(r0 * 1e6), word(r1 * 1e18)].concat());
        let swap = |a0: f64, a1out: f64| {
            log(11, jp, vec![T.solidly_swap, [0; 32], [0; 32]], [word(a0), word(0.0), word(0.0), word(a1out)].concat())
        };
        s.apply(&sync(13_000.0, 13_000_000.0), &store);
        let out = 13_000_000.0 / 13_001.0;
        s.apply(&sync(13_001.0, 13_000_000.0 - out), &store);
        s.apply(&swap(1e6, out * 1e18), &store);
        let (r0, r1) = reserves(&s, jp);
        s.apply(&sync(r0 + 30_000.0, r1 - 1e-6), &store);
        s.apply(&swap(30_000e6, 1e12), &store);
        let (r0, r1) = reserves(&s, jp);
        assert!(s.pools[0].price_native <= r0 / r1 * SPOT_SLACK, "{} vs {}", s.pools[0].price_native, r0 / r1);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_high_fee_v4_pool_is_not_thin_on_a_normal_trade() {
        // A 10%-fee v4 pool stored before we kept `spot`: its last recorded price includes the
        // fee, so measuring the next move from it read as a jump.
        let (store, mut s, dir) = fresh("old-v4");
        let (usdc, j, id) = (STABLES[0], [12u8; 20], [5u8; 32]);
        let pm = |block: u32, topic: [u8; 32], data: Vec<Vec<u8>>| {
            log(block, abi::V4_POOL_MANAGER, vec![topic, id, pad(&usdc), pad(&j)], data.concat())
        };
        let init = vec![word(100_000.0), word(60.0), word(0.0), word(1e6 * Q96), word(276_324.0)];
        s.apply(&pm(10, T.v4_initialize, init), &store);
        s.apply(&pm(10, T.v4_modify, vec![iword(-887_220.0), word(887_220.0), word(1e17), word(0.0)]), &store);
        let sw = |b: u32, usdc_in: f64, j_out: f64, sqrt: f64| {
            let data =
                vec![iword(-usdc_in), iword(j_out), word(sqrt * Q96), word(1e17), tick_at(sqrt), word(100_000.0)];
            pm(b, T.v4_swap, data)
        };
        s.apply(&sw(10, 1_000e6, 900e18, 1e6 * (1.0 - 4.5e-3)), &store);
        s.pools[0].spot = f64::NAN;
        s.pools[0].price_native = 1_000.0 / 900.0;
        s.apply(&sw(11, 100e6, 89.9e18, 1e6 * (1.0 - 4.5e-3 - 4.5e-4)), &store);
        assert_eq!(m_thin(&s, 0), 0);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
