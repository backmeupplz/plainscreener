//! Token icons and profiles (description and links).
//!
//! Sources, all free to store: CoinGecko's Base token list (logos), Trust Wallet's assets
//! repo (MIT: logos, descriptions, links for established tokens), and what launchpad
//! tokens declare on chain (Clanker's `imageUrl()`/`metadata()`, Zora's `tokenURI()`, …).
//! Every image is redrawn to a small PNG here, so we never serve fetched bytes as-is.

use crate::abi::{Addr, unhex};
use crate::rpc::Rpc;
use crate::state::NONE;
use crate::{App, og};
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Icon size in pixels (shown at 26–40 CSS px, so this is sharp at 2×).
const SIZE: u32 = 64;
const MAX_BYTES: usize = 2 << 20;
/// Seconds before looking again for a token that had no usable icon.
const RETRY: u32 = 86_400;
/// Seconds a profile is kept before it's looked up again.
const PROFILE_TTL: u32 = 7 * 86_400;
/// Only tokens with a pool at least this deep get looked up (the rest never reach a list).
const MIN_LIQ: f64 = 1_000.0;
const LIST: &str = "https://tokens.coingecko.com/base/all.json";
const TW_DIR: &str = "https://api.github.com/repos/trustwallet/assets/contents/blockchains/base/assets";
const TW_RAW: &str = "https://raw.githubusercontent.com/trustwallet/assets/master/blockchains/base/assets/";
const IPFS: [&str; 2] = ["https://magic.decentralized-content.com/ipfs/", "https://gateway.pinata.cloud/ipfs/"];
/// imageUrl(), image(), tokenURI(), contractURI(), metadata()
const GETTERS: [[u8; 4]; 5] = [
    [0xab, 0xa8, 0x31, 0x50],
    [0xf3, 0xcc, 0xaa, 0xc0],
    [0x3c, 0x13, 0x0d, 0x90],
    [0xe8, 0xa3, 0xd4, 0x85],
    [0x39, 0x2f, 0x37, 0xe9],
];

/// Token lists, refreshed daily: CoinGecko logo URLs, and Trust Wallet's folder names
/// (checksummed addresses) for the tokens it has.
#[derive(Default)]
struct Lists {
    logos: HashMap<Addr, String>,
    trust: HashMap<Addr, String>,
}

pub async fn run(app: Arc<App>) {
    let http = client();
    let rpc = Rpc::new(&app.cfg.rpc_url);
    let mut lists = Arc::new(Lists::default());
    let mut lists_at: Option<Instant> = None;
    // Tokens whose lookup failed for a passing reason (rate limit, timeout), and when.
    let mut backoff: HashMap<Addr, Instant> = HashMap::new();
    loop {
        if lists_at.is_none_or(|t| t.elapsed() > Duration::from_secs(86_400)) {
            match tokio::try_join!(coingecko(&http), trust(&http)) {
                Ok((logos, trust)) => (lists, lists_at) = (Arc::new(Lists { logos, trust }), Some(Instant::now())),
                Err(e) => eprintln!("icons: token lists: {e:#}"),
            }
        }
        // Without the lists, established tokens would be stored as having nothing.
        if lists_at.is_some() {
            backoff.retain(|_, at| at.elapsed() < Duration::from_secs(3600));
            let mut todo = todo(&app, &backoff).into_iter();
            let mut set = tokio::task::JoinSet::new();
            loop {
                while set.len() < 4
                    && let Some((t, addr, icon)) = todo.next()
                {
                    let (http, rpc, lists) = (http.clone(), rpc.clone(), lists.clone());
                    set.spawn(async move { (t, addr, find(&http, &rpc, &lists, addr, icon).await) });
                }
                let Some(done) = set.join_next().await else { break };
                let Ok((t, addr, found)) = done else { continue };
                let store = &app.sh.store;
                match found {
                    Ok(f) => {
                        let _ = store.profiles.insert(addr, f.profile.to_string());
                        match f.icon {
                            Some(png) => {
                                if store.icons.insert(addr, png).is_ok()
                                    && let Ok(mut s) = app.sh.state.write()
                                {
                                    s.tokens[t as usize].icon = true;
                                }
                            }
                            None if f.looked_for_icon => drop(store.icons.insert(addr, now().to_be_bytes())),
                            None => {}
                        }
                    }
                    Err(_) => drop(backoff.insert(addr, Instant::now())),
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn now() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// Up to 100 tokens to look up, with whether they still need an icon: never looked up
/// first, then due retries; deepest pools first within each.
fn todo(app: &App, backoff: &HashMap<Addr, Instant>) -> Vec<(u32, Addr, bool)> {
    let tokens: Vec<(f64, u32, Addr, bool)> = {
        let Ok(st) = app.sh.state.read() else { return Vec::new() };
        st.tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.best_pool != NONE && !backoff.contains_key(&t.addr))
            .map(|(i, t)| (st.liq(t.best_pool), i as u32, t.addr, t.icon))
            .filter(|(liq, ..)| *liq >= MIN_LIQ)
            .collect()
    };
    let now = now();
    let store = &app.sh.store;
    let mut out: Vec<(bool, f64, u32, Addr, bool)> = tokens
        .into_iter()
        .filter_map(|(liq, t, a, has_icon)| {
            // Icon: none stored, or a miss (4-byte timestamp) a day old.
            let icon = !has_icon
                && match store.icons.get(a) {
                    Ok(None) => true,
                    Ok(Some(v)) => v.len() == 4 && be32(&v) + RETRY < now,
                    Err(_) => false,
                };
            let profile_at = store.profiles.get(a).ok().flatten().map(|v| profile_time(&v));
            let new = profile_at.is_none();
            let due = profile_at.is_none_or(|at| at + PROFILE_TTL < now);
            (icon || due).then_some((!new, liq, t, a, icon))
        })
        .collect();
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
    out.into_iter().take(100).map(|(_, _, t, a, icon)| (t, a, icon)).collect()
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.get(..4).and_then(|b| b.try_into().ok()).unwrap_or_default())
}

fn profile_time(stored: &[u8]) -> u32 {
    serde_json::from_slice::<Value>(stored).ok().and_then(|v| v["t"].as_u64()).unwrap_or(0) as u32
}

struct Found {
    icon: Option<Vec<u8>>,
    /// Whether we tried to find an icon (a miss is then remembered).
    looked_for_icon: bool,
    /// `{"t": looked up at, "d": description, "l": [[kind, url], …]}`
    profile: Value,
}

/// Looks a token up in every source. `Err` when something failed that may work later
/// (node, rate limit, timeout), so nothing is stored.
async fn find(http: &reqwest::Client, rpc: &Rpc, lists: &Lists, addr: Addr, want_icon: bool) -> Result<Found> {
    let mut images: Vec<String> = lists.logos.get(&addr).cloned().into_iter().collect();
    let mut docs: Vec<Value> = Vec::new();
    if let Some(dir) = lists.trust.get(&addr) {
        images.push(format!("{TW_RAW}{dir}/logo.png"));
        if let Some(b) = get(http, &format!("{TW_RAW}{dir}/info.json")).await?
            && let Ok(v) = serde_json::from_slice(&b)
        {
            docs.push(v);
        }
    }
    let got = rpc.strings(addr, &GETTERS).await?;
    images.extend(got[..2].iter().flatten().cloned()); // imageUrl(), image()
    for uri in got[2..4].iter().flatten() {
        // tokenURI(), contractURI(): metadata JSON, or sometimes the image itself.
        if let Some(b) = get(http, uri).await? {
            match serde_json::from_slice::<Value>(&b) {
                Ok(v) => {
                    images.extend(v["image"].as_str().map(String::from));
                    docs.push(v);
                }
                Err(_) => images.push(uri.clone()),
            }
        }
    }
    if let Some(v) = got[4].as_deref().and_then(|m| serde_json::from_str(m).ok()) {
        docs.push(v); // metadata(): Clanker's JSON with description and social links
    }

    let mut icon = None;
    if want_icon {
        for url in images {
            let Some(bytes) = get(http, &url).await? else { continue };
            if let Ok(Some(png)) = tokio::task::spawn_blocking(move || og::icon_png(&bytes, SIZE)).await {
                icon = Some(png);
                break;
            }
        }
    }
    Ok(Found { icon, looked_for_icon: want_icon, profile: profile(&docs) })
}

/// Description and links from metadata documents, earlier documents first.
fn profile(docs: &[Value]) -> Value {
    let desc = docs
        .iter()
        .filter_map(|d| d["description"].as_str())
        .map(str::trim)
        .find(|d| !d.is_empty())
        .map(|d| d.chars().take(600).collect::<String>())
        .unwrap_or_default();
    let mut links: Vec<(&'static str, String)> = Vec::new();
    for d in docs {
        // A declared website before whatever URLs the document happens to list.
        let mut urls: Vec<&str> =
            ["website", "external_url", "websiteUrl"].iter().filter_map(|k| d[*k].as_str()).collect();
        strings(d, &mut urls);
        for url in urls {
            if let Some(kind) = link_kind(url)
                && !links.iter().any(|(k, _)| *k == kind)
            {
                links.push((kind, url.to_string()));
            }
        }
    }
    links.truncate(6);
    json!({"t": now(), "d": desc, "l": links})
}

/// Every string in a JSON document.
fn strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

/// What a link in token metadata points at, or `None` for ones not worth showing
/// (images, explorers, price sites, anything not plain HTTPS).
pub fn link_kind(url: &str) -> Option<&'static str> {
    if url.len() > 200 {
        return None;
    }
    let u = reqwest::Url::parse(url).ok().filter(|u| u.scheme() == "https" && u.username().is_empty())?;
    let host = u.host_str()?.trim_start_matches("www.");
    let path = u.path().to_ascii_lowercase();
    if [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"].iter().any(|e| path.ends_with(e)) {
        return None;
    }
    Some(match host {
        "x.com" | "twitter.com" => "X",
        "t.me" | "telegram.me" => "Telegram",
        "warpcast.com" | "farcaster.xyz" => "Farcaster",
        "discord.gg" | "discord.com" => "Discord",
        "github.com" => "GitHub",
        h if h.contains("scan.")
            || h.contains("ipfs")
            || ["coingecko.com", "coinmarketcap.com", "dexscreener.com", "geckoterminal.com"]
                .iter()
                .any(|s| h == *s || h.ends_with(&format!(".{s}"))) =>
        {
            return None;
        }
        _ => "Website",
    })
}

/// Contents of a `data:`, `ipfs://` or `https://` URL, up to `MAX_BYTES`; errors as `fetch`.
async fn get(http: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
    let url = url.trim();
    if let Some(d) = url.strip_prefix("data:") {
        let Some((meta, body)) = d.split_once(',') else { return Ok(None) };
        return Ok(if meta.ends_with(";base64") {
            STANDARD.decode(body.trim()).ok()
        } else {
            Some(body.as_bytes().to_vec())
        });
    }
    if let Some(path) = url.strip_prefix("ipfs://") {
        let path = path.trim_start_matches("ipfs/");
        let mut err = None;
        for g in IPFS {
            match fetch(http, &format!("{g}{path}")).await {
                Ok(Some(b)) => return Ok(Some(b)),
                Ok(None) => {}
                Err(e) => err = Some(e),
            }
        }
        return err.map_or(Ok(None), Err);
    }
    fetch(http, url).await
}

/// `Ok(None)` when the URL is not allowed, missing or too big; `Err` when the request
/// failed in a way that may pass (no answer, 429, 5xx).
async fn fetch(http: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
    let Some(url) = reqwest::Url::parse(url).ok().filter(allowed) else { return Ok(None) };
    let mut r = http.get(url).send().await?;
    let status = r.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        anyhow::bail!("{status}");
    }
    if !status.is_success() || r.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
        return Ok(None);
    }
    let mut out = Vec::new();
    while let Some(c) = r.chunk().await? {
        out.extend_from_slice(&c);
        if out.len() > MAX_BYTES {
            return Ok(None);
        }
    }
    Ok(Some(out))
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
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.filter(|a| public(a.ip())).collect();
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
            if a.previous().len() >= 3 || !allowed(a.url()) { a.stop() } else { a.follow() }
        }))
        .build()
        .unwrap_or_default()
}

/// Trust Wallet's Base tokens: address → folder name (the checksummed address).
async fn trust(http: &reqwest::Client) -> Result<HashMap<Addr, String>> {
    let v: Value = http.get(TW_DIR).send().await?.error_for_status()?.json().await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let name = e["name"].as_str()?;
            Some((unhex(name)?.try_into().ok()?, name.to_string()))
        })
        .collect())
}

/// CoinGecko's Base list: token → 250 px logo URL.
async fn coingecko(http: &reqwest::Client) -> Result<HashMap<Addr, String>> {
    let v: Value = http.get(LIST).send().await?.error_for_status()?.json().await?;
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
        assert!(allowed(&"https://assets.coingecko.com/a.png".parse().unwrap()));
    }

    #[test]
    fn redraws_rasters_only() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200"><rect width="300" height="200" fill="red"/></svg>"#;
        assert!(og::icon_png(svg.as_bytes(), SIZE).is_none());
        let png = og::icon_png(&og::png(svg).unwrap(), SIZE).unwrap();
        assert_eq!(imagesize::blob_size(&png).map(|d| (d.width, d.height)).ok(), Some((64, 64)));
    }
}
