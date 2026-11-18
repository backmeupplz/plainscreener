//! On-disk state in one fjall database. Keys are big-endian so ranges sort by number.

use crate::model::{Candle, Pool, Token, Trade};
use anyhow::Result;
use fjall::{Database, Keyspace, KeyspaceCreateOptions};

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
            db,
        })
    }

    /// The next block to index and the timestamp of the last indexed one.
    pub fn cursor(&self) -> Result<Option<(u32, u32)>> {
        Ok(self
            .meta
            .get("cursor")?
            .map(|v| (be32(&v[..4]), be32(v.get(4..8).unwrap_or_default()))))
    }

    pub fn set_cursor(&self, batch: &mut fjall::OwnedWriteBatch, block: u32, ts: u32) {
        batch.insert(
            &self.meta,
            "cursor",
            [block.to_be_bytes(), ts.to_be_bytes()].concat(),
        );
    }

    pub fn load_tokens(&self) -> Result<Vec<Token>> {
        self.tokens
            .range::<&[u8], _>(..)
            .map(|g| Ok(Token::decode(&g.into_inner()?.1)))
            .collect()
    }

    pub fn load_pools(&self) -> Result<Vec<Pool>> {
        self.pools
            .range::<&[u8], _>(..)
            .map(|g| Ok(Pool::decode(&g.into_inner()?.1)))
            .collect()
    }

    /// Up to `limit` candles before `before` (exclusive), oldest first.
    pub fn candles(
        &self,
        pool: u32,
        res: usize,
        from: u32,
        before: u32,
        limit: usize,
    ) -> Vec<(u32, Candle)> {
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
    pub fn trades(&self, pool: u32, before: (u32, u32), limit: usize) -> Vec<((u32, u32), Trade)> {
        self.trades
            .range(trade_key(pool, 0, 0)..trade_key(pool, before.0, before.1))
            .rev()
            .take(limit)
            .filter_map(|g| g.into_inner().ok())
            .map(|(k, v)| ((be32(&k[4..8]), be32(&k[8..12])), Trade::decode(&v)))
            .collect()
    }

    /// Trades newer than `after`, oldest first.
    pub fn trades_after(&self, pool: u32, after: (u32, u32)) -> Vec<((u32, u32), Trade)> {
        self.trades
            .range(trade_key(pool, after.0, after.1 + 1)..trade_key(pool + 1, 0, 0))
            .filter_map(|g| g.into_inner().ok())
            .map(|(k, v)| ((be32(&k[4..8]), be32(&k[8..12])), Trade::decode(&v)))
            .collect()
    }
}
