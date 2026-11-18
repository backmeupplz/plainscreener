//! On-chain safety checks for listed tokens, refreshed daily: who owns the contract, whether
//! it can be upgraded, whether its code has mint, pause or blacklist functions, and how much
//! of a v2-style pool's liquidity is burned. Heuristics from public data, not an audit.

use crate::abi::Kind;
use crate::abi::{Addr, hex, unhex};
use crate::rpc::Rpc;
use crate::state::NONE;
use crate::{App, abi};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIN_LIQ: f64 = 1_000.0;
const TTL: u32 = 86_400;
/// keccak("eip1967.proxy.implementation") - 1
const IMPL_SLOT: &str = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
const DEAD: Addr = abi::hex20("000000000000000000000000000000000000dead");
/// Function selectors looked for in the contract's code.
const MINT: [[u8; 4]; 2] = [[0x40, 0xc1, 0x0f, 0x19], [0xa0, 0x71, 0x2d, 0x68]]; // mint(address,uint256), mint(uint256)
const PAUSE: [[u8; 4]; 1] = [[0x84, 0x56, 0xcb, 0x59]]; // pause()
const BLACKLIST: [[u8; 4]; 4] = [
    [0xf9, 0xf9, 0x2b, 0xe4], // blacklist(address)
    [0x44, 0x33, 0x7e, 0xa1], // addToBlacklist(address)
    [0x15, 0x3b, 0x0d, 0x1e], // setBlacklist(address,bool)
    [0x0e, 0xcb, 0x93, 0xc0], // addBlackList(address)
];
const OWNER: &str = "0x8da5cb5b"; // owner()
const TOTAL_SUPPLY: &str = "0x18160ddd";
const BALANCE_OF: &str = "0x70a08231";

pub async fn run(app: Arc<App>) {
    let rpc = Rpc::new(&app.cfg.rpc_url);
    loop {
        for (addr, pool) in todo(&app) {
            match check(&rpc, addr, pool).await {
                Ok(v) => drop(app.sh.store.safety.insert(addr, v.to_string())),
                Err(e) => eprintln!("safety {}: {e:#}", hex(&addr)),
            }
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

fn now() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// Up to 50 listed tokens without a check from the last day, deepest pools first, each
/// with its best pool when that's a v2-style pool (whose LP token we can look at).
fn todo(app: &App) -> Vec<(Addr, Option<Addr>)> {
    let Ok(st) = app.sh.state.read() else { return Vec::new() };
    let now = now();
    let mut out: Vec<(f64, Addr, Option<Addr>)> = st
        .tokens
        .iter()
        .filter(|t| t.best_pool != NONE && st.liq(t.best_pool) >= MIN_LIQ)
        .filter(|t| {
            let at =
                app.sh.store.safety.get(t.addr).ok().flatten().and_then(|v| serde_json::from_slice::<Value>(&v).ok());
            at.and_then(|v| v["t"].as_u64()).is_none_or(|t| t as u32 + TTL < now)
        })
        .map(|t| {
            let pool = &st.pools[t.best_pool as usize];
            let lp =
                matches!(pool.kind, Kind::V2 | Kind::Solidly).then(|| pool.id[12..].try_into().unwrap_or_default());
            (st.liq(t.best_pool), t.addr, lp)
        })
        .collect();
    out.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter().take(50).map(|(_, a, p)| (a, p)).collect()
}

async fn eth_call(rpc: &Rpc, to: Addr, data: String) -> Result<Option<Vec<u8>>> {
    match rpc.call("eth_call", json!([{"to": hex(&to), "data": data}, "latest"])).await {
        Ok(v) => Ok(v.as_str().and_then(unhex)),
        Err(e) if e.is::<crate::rpc::RpcError>() => Ok(None), // reverted: no such function
        Err(e) => Err(e),
    }
}

async fn code(rpc: &Rpc, a: Addr) -> Result<Vec<u8>> {
    let v = rpc.call("eth_getCode", json!([hex(&a), "latest"])).await?;
    Ok(v.as_str().and_then(unhex).unwrap_or_default())
}

fn word_addr(w: &[u8]) -> Option<Addr> {
    w.get(12..32)?.try_into().ok()
}

fn amount(w: &[u8]) -> f64 {
    w.get(..32).and_then(|w| w.try_into().ok()).map(abi::u256_f64).unwrap_or(0.0)
}

/// Whether `code` pushes any of `sels` (how a contract's dispatcher matches functions).
fn has(code: &[u8], sels: &[[u8; 4]]) -> bool {
    code.windows(5).any(|w| w[0] == 0x63 && sels.iter().any(|s| w[1..] == s[..]))
}

/// The address a minimal proxy (EIP-1167) forwards to.
fn clone_target(code: &[u8]) -> Option<Addr> {
    let prefix = [0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
    code.strip_prefix(&prefix[..])?.get(..20)?.try_into().ok()
}

async fn check(rpc: &Rpc, token: Addr, lp: Option<Addr>) -> Result<Value> {
    let mut code = code(rpc, token).await?;
    if code.is_empty() {
        return Err(anyhow!("no code"));
    }
    // Upgradeable (EIP-1967) or a clone: the functions live in the implementation.
    let slot = rpc.call("eth_getStorageAt", json!([hex(&token), IMPL_SLOT, "latest"])).await?;
    let implementation = slot.as_str().and_then(unhex).and_then(|w| word_addr(&w)).filter(|a| *a != [0; 20]);
    let upgradeable = implementation.is_some();
    if let Some(target) = implementation.or_else(|| clone_target(&code)) {
        code = self::code(rpc, target).await?;
    }
    let owner = match eth_call(rpc, token, OWNER.into()).await?.and_then(|w| word_addr(&w)) {
        None => "none".to_string(),
        Some(a) if a == [0; 20] || a == DEAD => "renounced".to_string(),
        Some(a) => hex(&a),
    };
    let mut out = json!({
        "t": now(),
        "owner": owner,
        "upgradeable": upgradeable,
        "mint": has(&code, &MINT),
        "pause": has(&code, &PAUSE),
        "blacklist": has(&code, &BLACKLIST),
    });
    if let Some(pool) = lp {
        let supply = eth_call(rpc, pool, TOTAL_SUPPLY.into()).await?.map(|w| amount(&w)).unwrap_or(0.0);
        let mut burned = 0.0;
        for holder in [[0; 20], DEAD] {
            let data = format!("{BALANCE_OF}{}", &hex(&abi::pad(&holder))[2..]);
            burned += eth_call(rpc, pool, data).await?.map(|w| amount(&w)).unwrap_or(0.0);
        }
        if supply > 0.0 {
            out["lp"] = json!({"pool": hex(&pool), "burned": (burned / supply).min(1.0)});
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_selectors_and_clones() {
        // PUSH4 mint(address,uint256) in a dispatcher; a bare 4-byte match without PUSH4 doesn't count.
        assert!(has(&[0x80, 0x63, 0x40, 0xc1, 0x0f, 0x19, 0x14], &MINT));
        assert!(!has(&[0x40, 0xc1, 0x0f, 0x19], &MINT));
        let mut clone = vec![0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
        clone.extend([7u8; 20]);
        clone.extend([0x5a, 0xf4]);
        assert_eq!(clone_target(&clone), Some([7; 20]));
    }
}
