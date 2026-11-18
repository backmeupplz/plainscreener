//! What we keep about tokens, pools, candles and trades, plus their compact binary encoding.

use crate::abi::{Addr, Id, Kind, hex20};

/// Candle resolutions we store, in seconds. Shorter views are built from trades.
pub const RES: [u32; 6] = [60, 300, 900, 3600, 14_400, 86_400];
pub const RES_NAMES: [&str; 6] = ["1m", "5m", "15m", "1h", "4h", "1d"];

pub fn res_index(name: &str) -> Option<usize> {
    RES_NAMES.iter().position(|n| *n == name)
}

pub struct Token {
    pub addr: Addr,
    pub decimals: u8,
    pub symbol: String,
    pub name: String,
    /// Total supply in whole tokens.
    pub supply: f64,
    /// Trusted USD price (NaN until a pool with enough quote liquidity has traded).
    pub usd: f64,
    /// Pool that set `usd`, and its quote-side liquidity in USD at the time.
    pub ref_pool: u32,
    pub ref_liq: f64,
    /// Most liquid pool, where token links go.
    pub best_pool: u32,
    pub first_block: u32,
}

pub struct Pool {
    pub id: Id,
    pub kind: Kind,
    /// Factory (v2/v3/Solidly) or hooks (v4).
    pub origin: Addr,
    pub token0: u32,
    pub token1: u32,
    /// Whether token0 is the quote side (prices are base in quote).
    pub quote0: bool,
    /// Fee in pips (1e-6), when the creation event tells us.
    pub fee: u32,
    pub created_block: u32,
    pub created_ts: u32,
    /// Pool balances in whole tokens.
    pub bal0: f64,
    pub bal1: f64,
    /// sqrtPriceX96 for concentrated-liquidity pools.
    pub sqrt_price: f64,
    pub price_native: f64,
    pub price_usd: f64,
    pub last_ts: u32,
}

impl Pool {
    pub fn base(&self) -> u32 {
        if self.quote0 { self.token1 } else { self.token0 }
    }
    pub fn quote(&self) -> u32 {
        if self.quote0 { self.token0 } else { self.token1 }
    }
    pub fn bal_base(&self) -> f64 {
        if self.quote0 { self.bal1 } else { self.bal0 }.max(0.0)
    }
    pub fn bal_quote(&self) -> f64 {
        if self.quote0 { self.bal0 } else { self.bal1 }.max(0.0)
    }
    pub fn is_address(&self) -> bool {
        self.kind != Kind::V4
    }
    /// Pool id as shown in URLs: an address for v2/v3, the 32-byte id for v4.
    pub fn key(&self) -> String {
        crate::abi::hex(if self.is_address() { &self.id[12..] } else { &self.id })
    }
    pub fn dex(&self) -> &'static str {
        DEXES.iter().find(|(a, _)| *a == self.origin).map(|(_, n)| *n).unwrap_or(match self.kind {
            Kind::V2 => "v2 fork",
            Kind::Solidly => "Solidly fork",
            Kind::V3 => "v3 fork",
            Kind::V4 => "Uniswap v4",
        })
    }
}

/// Known factories and v4 hooks on Base. Anything else is labelled by its pool type.
const DEXES: [(Addr, &str); 7] = [
    (hex20("8909dc15e40173ff4699343b6eb8132c65e18ec6"), "Uniswap v2"),
    (hex20("33128a8fc17869897dce68ed026d694621f6fdfd"), "Uniswap v3"),
    (hex20("420dd381b31aef6683db6b902084cb0ffece40da"), "Aerodrome"),
    (hex20("5e7bb104d84c7cb9b682aac2f3d509f5f406809a"), "Aerodrome CL"),
    (hex20("0bfbcf9fa4f9c56b0f40a671ad40e0805a091865"), "PancakeSwap v3"),
    (hex20("71524b4f93c58fcbf659783284e38825f0622859"), "SushiSwap v2"),
    (hex20("c35dadb65012ec5796536bd9864ed8773abc74c4"), "SushiSwap v3"),
];

/// A stored candle: USD and native (quote) OHLC, USD volume, buy and sell counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Candle {
    pub o: f32,
    pub h: f32,
    pub l: f32,
    pub c: f32,
    pub no: f32,
    pub nh: f32,
    pub nl: f32,
    pub nc: f32,
    pub v: f32,
    pub buys: u32,
    pub sells: u32,
}

impl Candle {
    pub fn open(usd: f64, native: f64) -> Candle {
        let (u, n) = (usd as f32, native as f32);
        Candle { o: u, h: u, l: u, c: u, no: n, nh: n, nl: n, nc: n, ..Default::default() }
    }
    pub fn add(&mut self, usd: f64, native: f64, vol: f64, buy: bool) {
        let (u, n) = (usd as f32, native as f32);
        self.h = self.h.max(u);
        self.l = self.l.min(u);
        self.c = u;
        self.nh = self.nh.max(n);
        self.nl = self.nl.min(n);
        self.nc = n;
        if vol.is_finite() {
            self.v += vol as f32;
        }
        if buy { self.buys += 1 } else { self.sells += 1 }
    }
}

pub struct Trade {
    pub ts: u32,
    pub tx: [u8; 32],
    pub maker: Addr,
    pub buy: bool,
    /// Whole tokens of base and quote that moved.
    pub base: f64,
    pub quote: f64,
    pub usd: f32,
    pub price_usd: f32,
    pub price_native: f32,
}

/// Recent activity of a pool: 60 one-minute and 24 one-hour buckets.
pub struct Stats {
    pub m: [Bucket; 60],
    pub h: [Bucket; 24],
}

#[derive(Clone, Copy, Default)]
pub struct Bucket {
    pub t: u32,
    pub open: f32,
    pub vol: f32,
    pub buys: u32,
    pub sells: u32,
}

/// Totals over a window ending now.
#[derive(Clone, Copy, Default)]
pub struct Window {
    pub vol: f64,
    pub buys: u32,
    pub sells: u32,
    /// Price at the start of the window, if anything traded in it.
    pub open: f64,
}

impl Stats {
    pub fn new() -> Box<Stats> {
        Box::new(Stats { m: [Bucket::default(); 60], h: [Bucket::default(); 24] })
    }

    /// Adds a trade. `open` is the price before it (start of a new bucket).
    pub fn add(&mut self, ts: u32, open: f64, vol: f64, buy: bool) {
        for (ring, len) in [(&mut self.m[..], 60), (&mut self.h[..], 3600)] {
            let t = ts - ts % len;
            let b = &mut ring[(t / len) as usize % ring.len()];
            if b.t != t {
                *b = Bucket { t, open: open as f32, ..Default::default() };
            }
            b.bump(vol, buy);
        }
    }

    /// Restores a bucket from a stored candle.
    pub fn restore(&mut self, res: u32, t: u32, c: &Candle) {
        let ring = if res == 60 { &mut self.m[..] } else { &mut self.h[..] };
        let n = ring.len();
        ring[(t / res) as usize % n] = Bucket { t, open: c.o, vol: c.v, buys: c.buys, sells: c.sells };
    }

    /// Activity in the last `secs` seconds (5 min and 1 h use minutes, longer windows hours).
    pub fn window(&self, now: u32, secs: u32) -> Window {
        let (ring, len) = if secs <= 3600 { (&self.m[..], 60) } else { (&self.h[..], 3600) };
        let from = (now - now % len).saturating_sub(secs - len);
        let mut w = Window { open: f64::NAN, ..Default::default() };
        let mut first = u32::MAX;
        for b in ring.iter().filter(|b| b.t >= from && b.t <= now) {
            w.vol += b.vol as f64;
            w.buys += b.buys;
            w.sells += b.sells;
            if b.t < first {
                first = b.t;
                w.open = b.open as f64;
            }
        }
        w
    }

    pub fn last_active(&self) -> u32 {
        self.h.iter().map(|b| b.t).max().unwrap_or(0)
    }
}

impl Bucket {
    fn bump(&mut self, vol: f64, buy: bool) {
        if vol.is_finite() {
            self.vol += vol as f32;
        }
        if buy { self.buys += 1 } else { self.sells += 1 }
    }
}

// --- binary encoding -------------------------------------------------------------

#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn f32(mut self, v: f32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn f64(mut self, v: f64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
    pub fn str(self, s: &str) -> Self {
        let b = &s.as_bytes()[..s.len().min(255)];
        self.u8(b.len() as u8).bytes(b)
    }
}

/// Reader that yields zeroes past the end, so old records decode after fields are added.
pub struct R<'a>(pub &'a [u8]);

impl R<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0; N];
        let n = N.min(self.0.len());
        out[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        out
    }
    pub fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    pub fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take())
    }
    pub fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take())
    }
    pub fn f64(&mut self) -> f64 {
        f64::from_le_bytes(self.take())
    }
    pub fn arr<const N: usize>(&mut self) -> [u8; N] {
        self.take()
    }
    pub fn str(&mut self) -> String {
        let n = (self.u8() as usize).min(self.0.len());
        let s = String::from_utf8_lossy(&self.0[..n]).into_owned();
        self.0 = &self.0[n..];
        s
    }
}

impl Token {
    pub fn encode(&self) -> Vec<u8> {
        W::default()
            .bytes(&self.addr)
            .u8(self.decimals)
            .str(&self.symbol)
            .str(&self.name)
            .f64(self.supply)
            .f64(self.usd)
            .u32(self.ref_pool)
            .f64(self.ref_liq)
            .u32(self.best_pool)
            .u32(self.first_block)
            .0
    }
    pub fn decode(b: &[u8]) -> Token {
        let mut r = R(b);
        Token {
            addr: r.arr(),
            decimals: r.u8(),
            symbol: r.str(),
            name: r.str(),
            supply: r.f64(),
            usd: r.f64(),
            ref_pool: r.u32(),
            ref_liq: r.f64(),
            best_pool: r.u32(),
            first_block: r.u32(),
        }
    }
}

impl Pool {
    pub fn encode(&self) -> Vec<u8> {
        W::default()
            .bytes(&self.id)
            .u8(self.kind as u8)
            .bytes(&self.origin)
            .u32(self.token0)
            .u32(self.token1)
            .u8(self.quote0 as u8)
            .u32(self.fee)
            .u32(self.created_block)
            .u32(self.created_ts)
            .f64(self.bal0)
            .f64(self.bal1)
            .f64(self.sqrt_price)
            .f64(self.price_native)
            .f64(self.price_usd)
            .u32(self.last_ts)
            .0
    }
    pub fn decode(b: &[u8]) -> Pool {
        let mut r = R(b);
        Pool {
            id: r.arr(),
            kind: Kind::from_u8(r.u8()),
            origin: r.arr(),
            token0: r.u32(),
            token1: r.u32(),
            quote0: r.u8() == 1,
            fee: r.u32(),
            created_block: r.u32(),
            created_ts: r.u32(),
            bal0: r.f64(),
            bal1: r.f64(),
            sqrt_price: r.f64(),
            price_native: r.f64(),
            price_usd: r.f64(),
            last_ts: r.u32(),
        }
    }
}

impl Candle {
    pub fn encode(&self) -> Vec<u8> {
        [self.o, self.h, self.l, self.c, self.no, self.nh, self.nl, self.nc, self.v]
            .iter()
            .fold(W::default(), |w, &x| w.f32(x))
            .u32(self.buys)
            .u32(self.sells)
            .0
    }
    pub fn decode(b: &[u8]) -> Candle {
        let mut r = R(b);
        Candle {
            o: r.f32(),
            h: r.f32(),
            l: r.f32(),
            c: r.f32(),
            no: r.f32(),
            nh: r.f32(),
            nl: r.f32(),
            nc: r.f32(),
            v: r.f32(),
            buys: r.u32(),
            sells: r.u32(),
        }
    }
}

impl Trade {
    pub fn encode(&self) -> Vec<u8> {
        W::default()
            .u32(self.ts)
            .bytes(&self.tx)
            .bytes(&self.maker)
            .u8(self.buy as u8)
            .f64(self.base)
            .f64(self.quote)
            .f32(self.usd)
            .f32(self.price_usd)
            .f32(self.price_native)
            .0
    }
    pub fn decode(b: &[u8]) -> Trade {
        let mut r = R(b);
        Trade {
            ts: r.u32(),
            tx: r.arr(),
            maker: r.arr(),
            buy: r.u8() == 1,
            base: r.f64(),
            quote: r.f64(),
            usd: r.f32(),
            price_usd: r.f32(),
            price_native: r.f32(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let t = Token {
            addr: [1; 20],
            decimals: 6,
            symbol: "USDC".into(),
            name: "USD Coin".into(),
            supply: 1e9,
            usd: 1.0,
            ref_pool: 3,
            ref_liq: 5.0,
            best_pool: 7,
            first_block: 9,
        };
        let d = Token::decode(&t.encode());
        assert_eq!((d.symbol.as_str(), d.decimals, d.best_pool, d.first_block), ("USDC", 6, 7, 9));
        let c = Candle { o: 1.0, h: 2.0, l: 0.5, c: 1.5, v: 10.0, buys: 3, sells: 4, ..Default::default() };
        assert_eq!(Candle::decode(&c.encode()), c);
    }

    #[test]
    fn stats_windows() {
        let mut s = Stats::new();
        let now = 1_800_001_800; // half past an hour
        s.add(now - 7200, 1.0, 100.0, true); // two hours ago
        s.add(now - 120, 2.0, 50.0, false); // two minutes ago
        s.add(now, 3.0, 25.0, true);
        let w5 = s.window(now, 300);
        assert_eq!((w5.vol, w5.buys, w5.sells, w5.open), (75.0, 1, 1, 2.0));
        let w24 = s.window(now, 86_400);
        assert_eq!((w24.vol, w24.buys + w24.sells, w24.open), (175.0, 3, 1.0));
    }
}
