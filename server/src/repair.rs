//! One-off repair of candles stored before dust swaps stopped setting prices (see
//! `state::DUST_RAW`): a candle that saw one gets its prices rebuilt from the stored trades,
//! keeping its volume and counts.
// ponytail: delete once prod has run it ("dust-repair" in meta reads u32::MAX).

use crate::model::{Candle, RES, TradeKind};
use crate::state::DUST_RAW;
use crate::store::{Store, candle_key, trade_key};
use anyhow::Result;

/// (usd, native)
type Px = (f32, f32);

struct Acc {
    t: u32,
    o: Option<Px>,
    h: Px,
    l: Px,
    c: Option<Px>,
    /// Saw a dust swap, or opened at one.
    dust: bool,
}

impl Acc {
    fn new(t: u32, o: Option<Px>, dust: bool) -> Acc {
        let (h, l) = o.map_or(((f32::NEG_INFINITY, f32::NEG_INFINITY), (f32::INFINITY, f32::INFINITY)), |o| (o, o));
        Acc { t, o, h, l, c: None, dust }
    }
    fn add(&mut self, px: Px) {
        if self.o.is_none() {
            *self = Acc { dust: self.dust, ..Acc::new(self.t, Some(px), false) };
        }
        self.h = (self.h.0.max(px.0), self.h.1.max(px.1));
        self.l = (self.l.0.min(px.0), self.l.1.min(px.1));
        self.c = Some(px);
    }
}

const KEY: &str = "dust-repair";

/// Runs the repair in its own thread (a restart resumes at the next pool, and shutdown
/// doesn't wait for it). `scales[p]` is pool `p`'s (base, quote) raw units per whole token.
pub fn spawn(store: Store, scales: Vec<(f64, f64)>) {
    std::thread::spawn(move || {
        let r = (|| {
            let from = store.meta.get(KEY)?.map_or(0, |v| u32::from_be_bytes(v[..4].try_into().unwrap_or_default()));
            if from == u32::MAX {
                return Ok(0);
            }
            let until = store.cursor()?.map_or(0, |c| c.1);
            dust_wicks(&store, &scales, from, until)
        })();
        match r {
            Ok(0) => {}
            Ok(n) => eprintln!("dust repair: done, {n} candles fixed"),
            Err(e) => eprintln!("dust repair failed: {e}"),
        }
    });
}

/// Rebuilds the candles that closed by `until` and saw a dust swap, from pool `from` on.
/// Returns how many candles it rewrote.
fn dust_wicks(store: &Store, scales: &[(f64, f64)], from: u32, until: u32) -> Result<usize> {
    let mut b = store.db.batch();
    let (mut pending, mut fixed) = (0, 0);
    for (p, &(sb, sq)) in scales.iter().enumerate().skip(from as usize) {
        let p = p as u32;
        // Trades before a pool's first candle (a launch's seeding buy) never set a price.
        let Some(first) = store.candles.range(candle_key(p, 0, 0)..candle_key(p, 1, 0)).next() else { continue };
        let first = u32::from_be_bytes(first.key()?[5..9].try_into()?);
        let (mut last, mut last_dust): (Option<Px>, bool) = (None, false);
        let mut acc: [Option<Acc>; RES.len()] = Default::default();
        let mut flush = |ri: usize, a: &Acc, b: &mut fjall::OwnedWriteBatch| -> Result<()> {
            let (true, Some(o)) = (a.dust && a.t + RES[ri] <= until, a.o) else { return Ok(()) };
            let key = candle_key(p, ri, a.t);
            let Some(v) = store.candles.get(key)? else { return Ok(()) };
            let c = a.c.unwrap_or(o);
            let mut cd = Candle::decode(&v);
            (cd.o, cd.h, cd.l, cd.c) = (o.0, a.h.0, a.l.0, c.0);
            (cd.no, cd.nh, cd.nl, cd.nc) = (o.1, a.h.1, a.l.1, c.1);
            b.insert(&store.candles, key.to_vec(), cd.encode());
            (pending, fixed) = (pending + 1, fixed + 1);
            Ok(())
        };
        for g in store.trades.range(trade_key(p, 0, 0)..trade_key(p + 1, 0, 0)) {
            let (_, v) = g.into_inner()?;
            let t = crate::model::Trade::decode(&v);
            if t.kind != TradeKind::Swap || t.ts < first {
                continue;
            }
            let dust = (t.base * sb).min(t.quote * sq) < DUST_RAW;
            for (ri, res) in RES.iter().enumerate() {
                let bt = t.ts - t.ts % res;
                if acc[ri].as_ref().is_none_or(|a| a.t != bt) {
                    if let Some(a) = &acc[ri] {
                        flush(ri, a, &mut b)?;
                    }
                    acc[ri] = Some(Acc::new(bt, last, last_dust));
                }
                let a = acc[ri].as_mut().expect("just set");
                if dust { a.dust = true } else { a.add((t.price_usd, t.price_native)) }
            }
            if !dust {
                last = Some((t.price_usd, t.price_native));
            }
            last_dust = dust;
        }
        for (ri, a) in acc.iter().enumerate() {
            if let Some(a) = a {
                flush(ri, a, &mut b)?;
            }
        }
        if pending >= 10_000 || p.is_multiple_of(10_000) {
            b.insert(&store.meta, KEY, (p + 1).to_be_bytes());
            std::mem::replace(&mut b, store.db.batch()).commit()?;
            pending = 0;
        }
        if p.is_multiple_of(100_000) {
            eprintln!("dust repair: pool {p}/{}, {fixed} candles fixed", scales.len());
        }
    }
    b.insert(&store.meta, KEY, u32::MAX.to_be_bytes());
    b.commit()?;
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Trade;

    #[test]
    fn dust_wick_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!("ps-test-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.to_str().unwrap()).unwrap();
        // WETH/USDC: 1 WETH at 3000, then 23 wei for 1 raw USDC, then 1 WETH at 3010.
        let trades = [(0, 1.0, 3000.0), (10, 23e-18, 1e-6), (20, 1.0, 3010.0)];
        for (i, &(ts, base, quote)) in trades.iter().enumerate() {
            let px = (quote / base) as f32;
            let t = Trade {
                ts,
                tx: [0; 32],
                maker: [0; 20],
                buy: true,
                base,
                quote,
                usd: quote as f32,
                price_usd: px,
                price_native: px,
                kind: TradeKind::Swap,
            };
            store.trades.insert(trade_key(0, i as u32, 0), t.encode()).unwrap();
        }
        let wick = 1e-6 / 23e-18;
        let stored = Candle { o: 3000.0, h: wick, l: 3000.0, c: 3010.0, v: 6010.0, buys: 3, ..Default::default() };
        for ri in 0..RES.len() {
            store.candles.insert(candle_key(0, ri, 0), stored.encode()).unwrap();
        }
        // Only closed candles: the 1 m one, not the still open 5 m and longer.
        assert_eq!(dust_wicks(&store, &[(1e18, 1e6)], 0, 60).unwrap(), 1);
        let c = store.candles(0, 0, 0, u32::MAX, 1)[0].1;
        assert_eq!((c.o, c.h, c.l, c.c, c.nh, c.v, c.buys), (3000.0, 3010.0, 3000.0, 3010.0, 3010.0, 6010.0, 3));
        assert_eq!(store.candles(0, 1, 0, u32::MAX, 1)[0].1.h, wick, "open candle untouched");
        let _ = std::fs::remove_dir_all(dir);
    }
}
