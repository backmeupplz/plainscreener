//! On-disk state in one fjall database. Keys are big-endian so ranges sort by number.

use crate::model::{Candle, Pool, Token, Trade};
use anyhow::Result;
use fjall::{Database, Keyspace, KeyspaceCreateOptions};

/// A trade with its key: (block, log index).
pub type Keyed = ((u32, u32), Trade);

#[derive(Clone)]
pub struct Store {
    pub db: Database,
    meta: Keyspace,
    pub tokens: Keyspace,
    pub pools: Keyspace,
    /// [pool u32][res u8][t u32] → Candle
    pub candles: Keyspace,
    /// [pool u32][block u32][log index u32] → Trade
    pub trades: Keyspace,
    /// [token addr] → 64 px PNG, or the 4-byte Unix time of a failed lookup
    pub icons: Keyspace,
    /// [token addr] → JSON `{"t": looked up at, "d": description, "l": [[kind, url], …]}`
    pub profiles: Keyspace,
    /// [token addr] → JSON safety checks (see `safety`)
    pub safety: Keyspace,
}

pub fn candle_key(pool: u32, res: usize, t: u32) -> [u8; 9] {
    let mut k = [0; 9];
    k[..4].copy_from_slice(&pool.to_be_bytes());
    k[4] = res as u8;
    k[5..].copy_from_slice(&t.to_be_bytes());
    k
}

pub fn trade_key(pool: u32, block: u32, index: u32) -> [u8; 12] {
    let mut k = [0; 12];
    k[..4].copy_from_slice(&pool.to_be_bytes());
    k[4..8].copy_from_slice(&block.to_be_bytes());
    k[8..].copy_from_slice(&index.to_be_bytes());
    k
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap_or_default())
}

impl Store {
    pub fn open(dir: &str) -> Result<Store> {
        let db = Database::builder(dir).cache_size(2 << 30).open()?;
        let ks = |name| db.keyspace(name, KeyspaceCreateOptions::default);
        Ok(Store {
            meta: ks("meta")?,
            tokens: ks("tokens")?,
            pools: ks("pools")?,
            candles: ks("candles")?,
            trades: ks("trades")?,
            icons: ks("icons")?,
            profiles: ks("profiles")?,
            safety: ks("safety")?,
            db,
        })
    }

    /// The next block to index and the timestamp of the last indexed one.
    pub fn cursor(&self) -> Result<Option<(u32, u32)>> {
        Ok(self.meta.get("cursor")?.map(|v| (be32(&v[..4]), be32(v.get(4..8).unwrap_or_default()))))
    }

    pub fn set_cursor(&self, batch: &mut fjall::OwnedWriteBatch, block: u32, ts: u32) {
        batch.insert(&self.meta, "cursor", [block.to_be_bytes(), ts.to_be_bytes()].concat());
    }

    pub fn load_tokens(&self) -> Result<Vec<Token>> {
        self.tokens.range::<&[u8], _>(..).map(|g| Ok(Token::decode(&g.into_inner()?.1))).collect()
    }

    pub fn load_pools(&self) -> Result<Vec<Pool>> {
        self.pools.range::<&[u8], _>(..).map(|g| Ok(Pool::decode(&g.into_inner()?.1))).collect()
    }

    /// Up to `limit` candles before `before` (exclusive), oldest first.
    pub fn candles(&self, pool: u32, res: usize, from: u32, before: u32, limit: usize) -> Vec<(u32, Candle)> {
        let mut out: Vec<_> = self
            .candles
            .range(candle_key(pool, res, from)..candle_key(pool, res, before))
            .rev()
            .take(limit)
            .filter_map(|g| g.into_inner().ok())
            .map(|(k, v)| (be32(&k[5..9]), Candle::decode(&v)))
            .collect();
        out.reverse();
        out
    }

    /// Up to `limit` trades older than `before` = (block, index), newest first.
    /// Like `trades`, but only `maker`'s, looking at no more than `scan` trades. Also
    /// returns the oldest key looked at, where a further page would continue.
    pub fn trades_by(
        &self,
        pool: u32,
        maker: [u8; 20],
        before: (u32, u32),
        limit: usize,
        scan: usize,
    ) -> (Vec<Keyed>, Option<(u32, u32)>) {
        let mut out = Vec::new();
        let mut last = None;
        for g in self.trades.range(trade_key(pool, 0, 0)..trade_key(pool, before.0, before.1)).rev().take(scan) {
            let Ok((k, v)) = g.into_inner() else { continue };
            let key = (be32(&k[4..8]), be32(&k[8..12]));
            last = Some(key);
            let t = Trade::decode(&v);
            if t.maker == maker {
                out.push((key, t));
                if out.len() == limit {
                    break;
                }
            }
        }
        (out, last)
    }

    pub fn trades(&self, pool: u32, before: (u32, u32), limit: usize) -> Vec<((u32, u32), Trade)> {
        self.trades
            .range(trade_key(pool, 0, 0)..trade_key(pool, before.0, before.1))
            .rev()
            .take(limit)
            .filter_map(|g| g.into_inner().ok())
            .map(|(k, v)| ((be32(&k[4..8]), be32(&k[8..12])), Trade::decode(&v)))
            .collect()
    }

    /// Copies the data fetched from outside the chain (icons, profiles, safety checks)
    /// from another store, which a reindex would otherwise lose or fetch all over again.
    pub fn copy_offchain(&self, from: &Store) -> Result<usize> {
        let mut n = 0;
        for (src, dst) in [(&from.icons, &self.icons), (&from.profiles, &self.profiles), (&from.safety, &self.safety)] {
            for g in src.range::<&[u8], _>(..) {
                let (k, v) = g.into_inner()?;
                dst.insert(k, v)?;
                n += 1;
            }
        }
        self.db.persist(fjall::PersistMode::SyncAll)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_offchain_carries_icons_profiles_and_safety() {
        let dir = |n: &str| std::env::temp_dir().join(format!("ps-test-copy-{n}-{}", std::process::id()));
        let (old, new) =
            (Store::open(dir("old").to_str().unwrap()).unwrap(), Store::open(dir("new").to_str().unwrap()).unwrap());
        old.icons.insert([1u8; 20], b"png").unwrap();
        old.profiles.insert([2u8; 20], b"{}").unwrap();
        old.safety.insert([3u8; 20], b"{}").unwrap();
        old.tokens.insert([4u8; 20], b"chain data").unwrap();
        assert_eq!(new.copy_offchain(&old).unwrap(), 3);
        assert_eq!(&*new.icons.get([1u8; 20]).unwrap().unwrap(), b"png");
        assert!(new.profiles.get([2u8; 20]).unwrap().is_some() && new.safety.get([3u8; 20]).unwrap().is_some());
        assert!(new.tokens.get([4u8; 20]).unwrap().is_none());
        drop((old, new));
        let _ = (std::fs::remove_dir_all(dir("old")), std::fs::remove_dir_all(dir("new")));
    }
}
