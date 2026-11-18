//! The indexer. Old history comes from the SQD portal, the last ~1M blocks from our node
//! (it keeps ~1.3M), then the node block by block. Every source produces `RawLog`s that
//! go through the same `process` step.

use crate::abi::{Addr, RawLog};
use crate::rpc::Rpc;
use crate::sqd::Sqd;
use crate::{Config, Shared};
use anyhow::{Result, anyhow};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Blocks of history we take from our pruned node rather than SQD.
const NODE_HISTORY: u32 = 1_000_000;
/// Further behind the tip than this, fetch in parallel ranges instead of following.
const CATCH_UP: u32 = 1_000;

pub async fn run(cfg: Arc<Config>, sh: Shared) {
    let (rpc, sqd) = (Rpc::new(&cfg.rpc_url), Sqd::new(&cfg.sqd_url));
    let mut cursor = None;
    loop {
        if let Err(e) = step(&cfg, &sh, &rpc, &sqd, &mut cursor).await {
            eprintln!("indexer: {e:#}");
            // A failed catch-up may have committed chunks past the cursor we hold;
            // resume from what's stored so nothing is applied twice.
            cursor = None;
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

async fn step(
    cfg: &Config,
    sh: &Shared,
    rpc: &Rpc,
    sqd: &Sqd,
    cursor: &mut Option<u32>,
) -> Result<()> {
    let at = match *cursor {
        Some(c) => c,
        None => sh
            .store
            .cursor()?
            .map(|(block, _)| block)
            .unwrap_or(cfg.start_block),
    };
    let head = rpc.block_number().await?;
    let node_from = head.saturating_sub(NODE_HISTORY);
    let next = if at < node_from {
        let sqd = sqd.clone();
        catch_up(rpc, sh, at, node_from, 5_000, 4, move |a, b| {
            let sqd = sqd.clone();
            async move { sqd.logs(a, b).await }
        })
        .await?
    } else if head > at + CATCH_UP {
        let node = rpc.clone();
        catch_up(rpc, sh, at, head, 500, 8, move |a, b| {
            let node = node.clone();
            async move { node.logs(a, b).await }
        })
        .await?
    } else if head >= at {
        let logs = rpc.logs(at, head).await?;
        let touched = process(rpc, sh, logs, head + 1).await?;
        let _ = sh.live.send(Arc::new(touched));
        head + 1
    } else {
        tokio::time::sleep(Duration::from_millis(100)).await;
        at
    };
    *cursor = Some(next);
    Ok(())
}

/// Indexes `[from, to)` in `chunk`-block ranges, fetching up to `ahead` ranges at once
/// and applying them in order. Returns the next block to index.
async fn catch_up<F, Fut>(
    rpc: &Rpc,
    sh: &Shared,
    from: u32,
    to: u32,
    chunk: u32,
    ahead: usize,
    fetch: F,
) -> Result<u32>
where
    F: Fn(u32, u32) -> Fut,
    Fut: Future<Output = Result<Vec<RawLog>>> + Send + 'static,
{
    eprintln!("catching up {from}..{to}");
    let mut queue = VecDeque::new();
    let mut next = from;
    let mut progress = Progress::new(from);
    loop {
        while queue.len() < ahead && next < to {
            let end = (next + chunk).min(to);
            queue.push_back((end, tokio::spawn(fetch(next, end - 1))));
            next = end;
        }
        let Some((end, task)) = queue.pop_front() else {
            return Ok(to);
        };
        let logs = task.await??;
        let n = logs.len();
        let touched = process(rpc, sh, logs, end).await?;
        let _ = sh.live.send(Arc::new(touched));
        progress.tick(end, n, sh);
    }
}

/// Applies a batch of logs in order and persists the result with the next block to index.
async fn process(rpc: &Rpc, sh: &Shared, logs: Vec<RawLog>, next: u32) -> Result<Vec<u32>> {
    let unknown = sh
        .state
        .read()
        .map_err(|_| anyhow!("state lock poisoned"))?
        .unknown_tokens(&logs);
    let addrs: Vec<Addr> = unknown.iter().map(|(a, _)| *a).collect();
    let metas = if addrs.is_empty() {
        Vec::new()
    } else {
        rpc.meta(&addrs).await?
    };
    let mut s = sh
        .state
        .write()
        .map_err(|_| anyhow!("state lock poisoned"))?;
    for ((addr, block), meta) in unknown.into_iter().zip(metas) {
        s.add_token(addr, meta, block);
    }
    for log in &logs {
        s.apply(log, &sh.store);
    }
    s.flush(&sh.store, next)
}

struct Progress {
    at: Instant,
    block: u32,
    logs: usize,
}

impl Progress {
    fn new(block: u32) -> Progress {
        Progress {
            at: Instant::now(),
            block,
            logs: 0,
        }
    }

    fn tick(&mut self, block: u32, logs: usize, sh: &Shared) {
        self.logs += logs;
        let secs = self.at.elapsed().as_secs_f64();
        if secs < 30.0 {
            return;
        }
        if let Ok(s) = sh.state.read() {
            eprintln!(
                "block {block}: {:.0} blocks/s, {:.0} logs/s, {} pools, {} tokens",
                (block - self.block) as f64 / secs,
                self.logs as f64 / secs,
                s.pools.len(),
                s.tokens.len()
            );
        }
        *self = Progress::new(block);
    }
}
