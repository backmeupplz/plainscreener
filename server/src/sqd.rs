//! History from the public SQD portal: free, rate limited, ~900 blocks per response.

use crate::abi::{RawLog, T, V4_POOL_MANAGER, hex, unhex};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone)]
pub struct Sqd {
    url: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Block {
    header: Header,
    #[serde(default)]
    logs: Vec<Log>,
    #[serde(default)]
    transactions: Vec<Tx>,
}

#[derive(Deserialize)]
struct Header {
    number: u32,
    timestamp: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Log {
    log_index: u32,
    transaction_index: u32,
    transaction_hash: String,
    address: String,
    topics: Vec<String>,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tx {
    transaction_index: u32,
    from: String,
}

impl Sqd {
    pub fn new(url: &str) -> Sqd {
        Sqd { url: format!("{url}/stream"), http: reqwest::Client::new() }
    }

    /// Our logs in `[from, to]`, ordered, with block timestamps and transaction senders.
    pub async fn logs(&self, from: u32, to: u32) -> Result<Vec<RawLog>> {
        let topics = |ts: Vec<[u8; 32]>| ts.iter().map(|t| hex(t)).collect::<Vec<_>>();
        let mut out = Vec::new();
        let mut next = from;
        while next <= to {
            let body = json!({
                "type": "evm",
                "fromBlock": next,
                "toBlock": to,
                "fields": {
                    "block": {"number": true, "timestamp": true},
                    "log": {"logIndex": true, "transactionIndex": true, "transactionHash": true, "address": true, "topics": true, "data": true},
                    "transaction": {"transactionIndex": true, "from": true}
                },
                "logs": [
                    {"topic0": topics(T.any_address()), "transaction": true},
                    {"address": [hex(&V4_POOL_MANAGER)], "topic0": topics(T.v4()), "transaction": true}
                ]
            });
            let resp = match self.http.post(&self.url).json(&body).timeout(Duration::from_secs(120)).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("sqd {next}: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let status = resp.status();
            if status.as_u16() == 429 || status.is_server_error() {
                // The public portal answers 529 "overloaded" with a retry-after.
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(5);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let text = resp.error_for_status()?.text().await?;
            let mut last = None;
            for line in text.lines().filter(|l| !l.is_empty()) {
                let b: Block = serde_json::from_str(line).context("sqd block")?;
                let senders: HashMap<u32, &str> = b.transactions.iter().map(|t| (t.transaction_index, t.from.as_str())).collect();
                for l in &b.logs {
                    out.push(RawLog {
                        block: b.header.number,
                        ts: b.header.timestamp,
                        index: l.log_index,
                        address: unhex(&l.address).and_then(|a| a.try_into().ok()).context("log address")?,
                        topics: l.topics.iter().filter_map(|t| unhex(t)?.try_into().ok()).collect(),
                        data: unhex(&l.data).unwrap_or_default(),
                        tx: unhex(&l.transaction_hash).and_then(|h| h.try_into().ok()).context("tx hash")?,
                        from: senders
                            .get(&l.transaction_index)
                            .and_then(|f| unhex(f)?.try_into().ok())
                            .unwrap_or_default(),
                    });
                }
                last = Some(b.header.number);
            }
            // The portal always ends a range with its last block's header, so an empty
            // answer means something went wrong, not that the range is done.
            next = last.context("empty sqd response")? + 1;
        }
        Ok(out)
    }
}
