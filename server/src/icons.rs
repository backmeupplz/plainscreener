//! Token icons. Established tokens get theirs from CoinGecko's Base token list; launchpad
//! tokens declare one on chain (Clanker's `imageUrl()`, Zora's `tokenURI()` metadata, …).
//! Every image is redrawn to a small PNG here, so we never serve fetched bytes as-is.

use crate::abi::{Addr, unhex};
use crate::rpc::Rpc;
use crate::state::NONE;
use crate::{App, og};
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Icon size in pixels (shown at 26–40 CSS px, so this is sharp at 2×).
const SIZE: u32 = 64;
const MAX_BYTES: usize = 2 << 20;
/// Seconds before looking again for a token that had no usable icon.
const RETRY: u32 = 86_400;
/// Only tokens with a pool at least this deep get an icon (the rest never reach a list).
const MIN_LIQ: f64 = 1_000.0;
const LIST: &str = "https://tokens.coingecko.com/base/all.json";
const IPFS: [&str; 2] = [
    "https://magic.decentralized-content.com/ipfs/",
    "https://gateway.pinata.cloud/ipfs/",
];
/// imageUrl(), image(), tokenURI(), contractURI()
const GETTERS: [[u8; 4]; 4] = [
    [0xab, 0xa8, 0x31, 0x50],
    [0xf3, 0xcc, 0xaa, 0xc0],
    [0x3c, 0x13, 0x0d, 0x90],
    [0xe8, 0xa3, 0xd4, 0x85],
];

pub async fn run(app: Arc<App>) {
    let http = client();
    let rpc = Rpc::new(&app.cfg.rpc_url);
    let mut list = Arc::new(HashMap::new());
    let mut list_at: Option<Instant> = None;
    loop {
        if list_at.is_none_or(|t| t.elapsed() > Duration::from_secs(86_400)) {
            match coingecko(&http).await {
                Ok(l) => (list, list_at) = (Arc::new(l), Some(Instant::now())),
                Err(e) => eprintln!("icons: token list: {e:#}"),
            }
        }
        let mut set = tokio::task::JoinSet::new();
        for (t, addr) in todo(&app) {
            if set.len() >= 4 {
                set.join_next().await;
            }
            let (app, http, rpc, list) = (app.clone(), http.clone(), rpc.clone(), list.clone());
            set.spawn(async move {
                match find(&http, &rpc, &list, addr).await {
                    Ok(Some(png)) => {
                        if app.sh.store.icons.insert(addr, png).is_ok()
                            && let Ok(mut s) = app.sh.state.write()
                        {
                            s.tokens[t as usize].icon = true;
                        }
                    }
                    Ok(None) => drop(app.sh.store.icons.insert(addr, now().to_be_bytes())),
                    Err(_) => {} // node unreachable: try again next round
                }
            });
        }
        while set.join_next().await.is_some() {}
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Up to 100 tokens worth an icon that we haven't tried lately, deepest pools first.
fn todo(app: &App) -> Vec<(u32, Addr)> {
    let mut out: Vec<(f64, u32, Addr)> = {
        let Ok(st) = app.sh.state.read() else {
            return Vec::new();
        };
        st.tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.icon && t.best_pool != NONE)
            .map(|(i, t)| (st.liq(t.best_pool), i as u32, t.addr))
            .filter(|(liq, ..)| *liq >= MIN_LIQ)
            .collect()
    };
    let cutoff = now().saturating_sub(RETRY);
    out.retain(|(.., a)| match app.sh.store.icons.get(a) {
        Ok(Some(v)) => {
            v.len() == 4 && u32::from_be_bytes(v[..4].try_into().unwrap_or_default()) < cutoff
        }
        Ok(None) => true,
        Err(_) => false,
    });
    out.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter().take(100).map(|(_, t, a)| (t, a)).collect()
}

/// The first usable icon from the token list or the token's own getters.
/// Errors only when the node can't be reached.
async fn find(
    http: &reqwest::Client,
    rpc: &Rpc,
    list: &HashMap<Addr, String>,
    addr: Addr,
) -> Result<Option<Vec<u8>>> {
    let mut urls: Vec<String> = list.get(&addr).cloned().into_iter().collect();
    urls.extend(rpc.strings(addr, &GETTERS).await?.into_iter().flatten());
    for url in urls {
        let Some(mut bytes) = get(http, &url).await else {
            continue;
        };
        // Metadata JSON (tokenURI and friends) points at the image.
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let Some(img) = v["image"].as_str() else {
                continue;
            };
            let Some(b) = get(http, img).await else {
                continue;
            };
            bytes = b;
        }
        if let Ok(Some(png)) = tokio::task::spawn_blocking(move || og::icon_png(&bytes, SIZE)).await
        {
            return Ok(Some(png));
        }
    }
    Ok(None)
}

/// Contents of a `data:`, `ipfs://` or `https://` URL, up to `MAX_BYTES`.
async fn get(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let url = url.trim();
    if let Some(d) = url.strip_prefix("data:") {
        let (meta, body) = d.split_once(',')?;
        return if meta.ends_with(";base64") {
            STANDARD.decode(body.trim()).ok()
        } else {
            Some(body.as_bytes().to_vec())
        };
    }
    if let Some(path) = url.strip_prefix("ipfs://") {
        let path = path.trim_start_matches("ipfs/");
        for g in IPFS {
            if let Some(b) = fetch(http, &format!("{g}{path}")).await {
                return Some(b);
            }
        }
        return None;
    }
    fetch(http, url).await
}

async fn fetch(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let url = reqwest::Url::parse(url).ok().filter(allowed)?;
    let mut r = http.get(url).send().await.ok()?.error_for_status().ok()?;
    if r.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
        return None;
    }
    let mut out = Vec::new();
    while let Some(c) = r.chunk().await.ok()? {
        out.extend_from_slice(&c);
        if out.len() > MAX_BYTES {
            return None;
        }
    }
    Some(out)
}

/// Token URLs are anyone's to choose, and this box shares a network with the Base node:
/// only HTTPS to a named host, and only hosts that resolve to public addresses.
fn allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.domain().is_some()
}

fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_unspecified()
                || v.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && o[1] & 0xc0 == 64) // carrier-grade NAT, Tailscale
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && o[1] & 0xfe == 18))
        }
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(v4) => public(IpAddr::V4(v4)),
            None => {
                let s = v.segments()[0];
                !(v.is_loopback()
                    || v.is_unspecified()
                    || v.is_multicast()
                    || s & 0xfe00 == 0xfc00
                    || s & 0xffc0 == 0xfe80)
            }
        },
    }
}

struct PublicDns;

impl reqwest::dns::Resolve for PublicDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicDns))
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .user_agent("plainscreener (+https://plainscreener.com)")
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() >= 3 || !allowed(a.url()) {
                a.stop()
            } else {
                a.follow()
            }
        }))
        .build()
        .unwrap_or_default()
}

/// CoinGecko's Base list: token → 250 px logo URL.
async fn coingecko(http: &reqwest::Client) -> Result<HashMap<Addr, String>> {
    let v: serde_json::Value = http
        .get(LIST)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["tokens"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let a: Addr = unhex(t["address"].as_str()?)?.try_into().ok()?;
            Some((a, t["logoURI"].as_str()?.replace("/thumb/", "/large/")))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses() {
        for ip in [
            "10.0.0.1",
            "172.18.0.2",
            "192.168.1.1",
            "127.0.0.1",
            "100.73.59.12",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fd7a:115c::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "213.133.101.30", "2606:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
        assert!(!allowed(&"http://example.com/a.png".parse().unwrap()));
        assert!(!allowed(&"https://10.0.0.1/a.png".parse().unwrap()));
        assert!(allowed(
            &"https://assets.coingecko.com/a.png".parse().unwrap()
        ));
    }
}
