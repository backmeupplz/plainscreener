//! DEX events we index, decoded from raw logs.
//!
//! Pools are discovered from factory creation events emitted by *any* contract, so every
//! fork of Uniswap v2/v3 and Solidly (Aerodrome, Velodrome forks) is covered without a
//! factory allowlist. Uniswap v4 events are only accepted from the canonical PoolManager.

use std::sync::LazyLock;
use tiny_keccak::{Hasher, Keccak};

pub type Addr = [u8; 20];
/// Pool id: v4 pool id, or a v2/v3 pool address left-padded to 32 bytes.
pub type Id = [u8; 32];

pub const V4_POOL_MANAGER: Addr = hex20("498581ff718922c3f8e6a244956af099b2652b2b");
pub const WETH: Addr = hex20("4200000000000000000000000000000000000006");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    V2 = 0,
    Solidly = 1,
    V3 = 2,
    V4 = 3,
}

impl Kind {
    pub fn from_u8(b: u8) -> Kind {
        match b {
            1 => Kind::Solidly,
            2 => Kind::V3,
            3 => Kind::V4,
            _ => Kind::V2,
        }
    }
}

/// One log in the shape every source (HyperSync, our node) is converted to.
#[derive(Clone, Debug, Default)]
pub struct RawLog {
    pub block: u32,
    /// Block timestamp (Base's block time has changed over the years, so it's carried along).
    pub ts: u32,
    pub index: u32,
    pub address: Addr,
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
    pub tx: [u8; 32],
    pub from: Addr,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// `origin` is the factory (v2/v3/Solidly) or the hooks contract (v4); it names the DEX.
    Created { kind: Kind, pool: Id, token0: Addr, token1: Addr, fee: u32, sqrt_price: f64, origin: Addr },
    /// v2/Solidly reserves after a change, raw token units.
    Sync { r0: f64, r1: f64 },
    /// v2/Solidly swap, raw units: pool balance deltas.
    SwapV2 { d0: f64, d1: f64 },
    /// Concentrated-liquidity swap: pool balance deltas (raw), price after, LP fee in pips (v4).
    SwapCl { d0: f64, d1: f64, sqrt_price: f64, fee: u32 },
    /// v3 Collect: tokens leaving the pool (withdrawn liquidity and fees).
    Flow { d0: f64, d1: f64 },
    /// Liquidity added or removed, raw amounts (positive). `moves`: the pool's balances
    /// change by them (v3 Mint); otherwise a Sync or Collect accounts for the tokens.
    Liquidity { d0: f64, d1: f64, add: bool, moves: bool },
    /// v4 liquidity change; token amounts depend on the current price.
    Modify { tick_lower: i32, tick_upper: i32, liquidity: f64 },
}

pub fn keccak(s: impl AsRef<[u8]>) -> [u8; 32] {
    let mut k = Keccak::v256();
    k.update(s.as_ref());
    let mut out = [0; 32];
    k.finalize(&mut out);
    out
}

pub struct Topics {
    pub v2_pair_created: [u8; 32],
    pub solidly_pool_created: [u8; 32],
    pub v3_pool_created: [u8; 32],
    pub slipstream_pool_created: [u8; 32],
    pub v4_initialize: [u8; 32],
    pub v2_sync: [u8; 32],
    pub solidly_sync: [u8; 32],
    pub v2_swap: [u8; 32],
    pub solidly_swap: [u8; 32],
    pub v3_swap: [u8; 32],
    pub pancake_swap: [u8; 32],
    pub v3_mint: [u8; 32],
    pub v3_collect: [u8; 32],
    pub v2_mint: [u8; 32],
    pub v2_burn: [u8; 32],
    pub solidly_burn: [u8; 32],
    pub v3_burn: [u8; 32],
    pub v4_swap: [u8; 32],
    pub v4_modify: [u8; 32],
}

pub static T: LazyLock<Topics> = LazyLock::new(|| Topics {
    v2_pair_created: keccak("PairCreated(address,address,address,uint256)"),
    solidly_pool_created: keccak("PoolCreated(address,address,bool,address,uint256)"),
    v3_pool_created: keccak("PoolCreated(address,address,uint24,int24,address)"),
    slipstream_pool_created: keccak("PoolCreated(address,address,int24,address)"),
    v4_initialize: keccak("Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)"),
    v2_sync: keccak("Sync(uint112,uint112)"),
    solidly_sync: keccak("Sync(uint256,uint256)"),
    v2_swap: keccak("Swap(address,uint256,uint256,uint256,uint256,address)"),
    solidly_swap: keccak("Swap(address,address,uint256,uint256,uint256,uint256)"),
    v3_swap: keccak("Swap(address,address,int256,int256,uint160,uint128,int24)"),
    pancake_swap: keccak("Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)"),
    v3_mint: keccak("Mint(address,address,int24,int24,uint128,uint256,uint256)"),
    v3_collect: keccak("Collect(address,address,int24,int24,uint128,uint128)"),
    // Solidly pools emit the same Mint as v2.
    v2_mint: keccak("Mint(address,uint256,uint256)"),
    v2_burn: keccak("Burn(address,uint256,uint256,address)"),
    solidly_burn: keccak("Burn(address,address,uint256,uint256)"),
    v3_burn: keccak("Burn(address,int24,int24,uint128,uint256,uint256)"),
    v4_swap: keccak("Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)"),
    v4_modify: keccak("ModifyLiquidity(bytes32,address,int24,int24,int256,bytes32)"),
});

impl Topics {
    /// Topics accepted from any address.
    pub fn any_address(&self) -> Vec<[u8; 32]> {
        vec![
            self.v2_pair_created,
            self.solidly_pool_created,
            self.v3_pool_created,
            self.slipstream_pool_created,
            self.v2_sync,
            self.solidly_sync,
            self.v2_swap,
            self.solidly_swap,
            self.v3_swap,
            self.pancake_swap,
            self.v3_mint,
            self.v3_collect,
            self.v2_mint,
            self.v2_burn,
            self.solidly_burn,
            self.v3_burn,
        ]
    }
    /// Topics accepted only from the v4 PoolManager.
    pub fn v4(&self) -> Vec<[u8; 32]> {
        vec![self.v4_initialize, self.v4_swap, self.v4_modify]
    }
}

/// Decodes a log into the pool it belongs to and what happened.
pub fn decode(log: &RawLog) -> Option<(Id, Event)> {
    let t = &*T;
    let t0 = *log.topics.first()?;
    let w = |i: usize| word(&log.data, i);
    let own = pad(&log.address);
    let ev = if t0 == t.v2_pair_created || t0 == t.solidly_pool_created {
        let kind = if t0 == t.v2_pair_created { Kind::V2 } else { Kind::Solidly };
        let pool = pad(&addr(w(0)?));
        return created(log, kind, pool, 0, 0.0);
    } else if t0 == t.v3_pool_created {
        let fee = u256_f64(log.topics.get(3)?) as u32;
        return created(log, Kind::V3, pad(&addr(w(1)?)), fee, 0.0);
    } else if t0 == t.slipstream_pool_created {
        return created(log, Kind::V3, pad(&addr(w(0)?)), 0, 0.0);
    } else if t0 == t.v2_sync || t0 == t.solidly_sync {
        Event::Sync { r0: u256_f64(w(0)?), r1: u256_f64(w(1)?) }
    } else if t0 == t.v2_swap || t0 == t.solidly_swap {
        Event::SwapV2 { d0: u256_f64(w(0)?) - u256_f64(w(2)?), d1: u256_f64(w(1)?) - u256_f64(w(3)?) }
    } else if t0 == t.v3_swap || t0 == t.pancake_swap {
        Event::SwapCl { d0: i256_f64(w(0)?), d1: i256_f64(w(1)?), sqrt_price: u256_f64(w(2)?), fee: 0 }
    } else if t0 == t.v3_mint {
        Event::Liquidity { d0: u256_f64(w(2)?), d1: u256_f64(w(3)?), add: true, moves: true }
    } else if t0 == t.v3_burn {
        Event::Liquidity { d0: u256_f64(w(1)?), d1: u256_f64(w(2)?), add: false, moves: false }
    } else if t0 == t.v2_mint || t0 == t.v2_burn {
        Event::Liquidity { d0: u256_f64(w(0)?), d1: u256_f64(w(1)?), add: t0 == t.v2_mint, moves: false }
    } else if t0 == t.solidly_burn {
        Event::Liquidity { d0: u256_f64(w(0)?), d1: u256_f64(w(1)?), add: false, moves: false }
    } else if t0 == t.v3_collect {
        Event::Flow { d0: -u256_f64(w(1)?), d1: -u256_f64(w(2)?) }
    } else if log.address == V4_POOL_MANAGER {
        let id = *log.topics.get(1)?;
        let ev = if t0 == t.v4_swap {
            // v4 reports the swapper's deltas; the pool's are the opposite.
            Event::SwapCl {
                d0: -i256_f64(w(0)?),
                d1: -i256_f64(w(1)?),
                sqrt_price: u256_f64(w(2)?),
                fee: u256_f64(w(5)?) as u32,
            }
        } else if t0 == t.v4_modify {
            Event::Modify { tick_lower: i256_i32(w(0)?), tick_upper: i256_i32(w(1)?), liquidity: i256_f64(w(2)?) }
        } else if t0 == t.v4_initialize {
            let (c0, c1) = (addr(log.topics.get(2)?), addr(log.topics.get(3)?));
            let ev = Event::Created {
                kind: Kind::V4,
                pool: id,
                token0: eth_as_weth(c0),
                token1: eth_as_weth(c1),
                fee: u256_f64(w(0)?) as u32,
                sqrt_price: u256_f64(w(3)?),
                origin: addr(w(2)?),
            };
            return Some((id, ev));
        } else {
            return None;
        };
        return Some((id, ev));
    } else {
        return None;
    };
    Some((own, ev))
}

fn created(log: &RawLog, kind: Kind, pool: Id, fee: u32, sqrt_price: f64) -> Option<(Id, Event)> {
    let token0 = addr(log.topics.get(1)?);
    let token1 = addr(log.topics.get(2)?);
    Some((pool, Event::Created { kind, pool, token0, token1, fee, sqrt_price, origin: log.address }))
}

/// v4 uses address(0) for native ETH; we price and display it as WETH.
// ponytail: ETH pools show as WETH; add a separate ETH token if anyone cares.
fn eth_as_weth(a: Addr) -> Addr {
    if a == [0; 20] { WETH } else { a }
}

fn word(data: &[u8], i: usize) -> Option<&[u8; 32]> {
    data.get(i * 32..i * 32 + 32)?.try_into().ok()
}

pub fn addr(w: &[u8; 32]) -> Addr {
    w[12..].try_into().unwrap_or_default()
}

pub fn pad(a: &Addr) -> Id {
    let mut id = [0; 32];
    id[12..].copy_from_slice(a);
    id
}

pub fn u256_f64(w: &[u8; 32]) -> f64 {
    w.iter().fold(0.0, |acc, &b| acc * 256.0 + b as f64)
}

/// Two's-complement int256 as f64.
pub fn i256_f64(w: &[u8; 32]) -> f64 {
    if w[0] & 0x80 == 0 {
        return u256_f64(w);
    }
    // -(~w + 1): invert, then add one while folding.
    let inv: [u8; 32] = std::array::from_fn(|i| !w[i]);
    -(u256_f64(&inv) + 1.0)
}

fn i256_i32(w: &[u8; 32]) -> i32 {
    i32::from_be_bytes(w[28..].try_into().unwrap_or_default())
}

pub const fn hex20(s: &str) -> Addr {
    let b = s.as_bytes();
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("bad hex"),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// Parses "0x…" hex of any length into bytes.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(hex_str: &str) -> [u8; 32] {
        unhex(hex_str).unwrap().try_into().unwrap()
    }

    #[test]
    fn known_topics() {
        assert_eq!(hex(&T.v2_swap), "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");
        assert_eq!(hex(&T.v3_swap), "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
        assert_eq!(hex(&T.v2_sync), "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
        assert_eq!(hex(&T.v2_pair_created), "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9");
        assert_eq!(hex(&T.v3_pool_created), "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118");
    }

    #[test]
    fn ints() {
        let neg_one = [0xff; 32];
        assert_eq!(i256_f64(&neg_one), -1.0);
        assert_eq!(i256_i32(&neg_one), -1);
        let mut five = [0; 32];
        five[31] = 5;
        assert_eq!(u256_f64(&five), 5.0);
        assert_eq!(i256_f64(&five), 5.0);
        assert_eq!(u256_f64(&w("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")), 1e18);
    }

    #[test]
    fn v3_swap_and_foreign_v4() {
        let mut data = Vec::new();
        let mut neg = [0xff; 32];
        neg[31] = 0x9c; // -100
        data.extend_from_slice(&neg);
        let mut pos = [0; 32];
        pos[31] = 50;
        data.extend_from_slice(&pos);
        data.extend_from_slice(&pos); // sqrtPrice
        data.extend_from_slice(&[0; 64]); // liquidity, tick
        let log = RawLog { topics: vec![T.v3_swap, [0; 32], [0; 32]], data, address: [7; 20], ..Default::default() };
        let (id, ev) = decode(&log).unwrap();
        assert_eq!(id, pad(&[7; 20]));
        assert_eq!(ev, Event::SwapCl { d0: -100.0, d1: 50.0, sqrt_price: 50.0, fee: 0 });
        let fake_v4 = RawLog { topics: vec![T.v4_swap, [1; 32]], data: vec![0; 192], ..Default::default() };
        assert!(decode(&fake_v4).is_none(), "v4 events only from the PoolManager");
    }
}
