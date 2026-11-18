//! plainscreener: one process that indexes Base DEX swaps and serves the site.

mod abi;
mod http;
mod index;
mod model;
mod og;
mod pages;
mod rpc;
mod sqd;
mod state;
mod store;

use anyhow::Result;
use state::State;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use store::Store;
use tokio::sync::broadcast;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub const JS: &str = include_str!("../../web/app.js");
/// Built by `cargo build -p chart --profile wasm --target wasm32-unknown-unknown`
/// and copied here (see the Dockerfile / `make`).
pub const WASM: &[u8] = include_bytes!("../../web/chart.wasm");

pub struct Config {
    pub data_dir: String,
    pub rpc_url: String,
    pub sqd_url: String,
    pub start_block: u32,
    pub listen: String,
    pub public_url: String,
    pub index: bool,
}

impl Config {
    fn from_env() -> Config {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Config {
            data_dir: var("DATA_DIR", "data"),
            rpc_url: var("RPC_URL", "http://127.0.0.1:8545"),
            sqd_url: var("SQD_URL", "https://portal.sqd.dev/datasets/base-mainnet"),
            start_block: var("START_BLOCK", "0").parse().unwrap_or(0),
            listen: var("LISTEN", "0.0.0.0:8080"),
            public_url: var("PUBLIC_URL", "https://plainscreener.com").trim_end_matches('/').to_string(),
            index: var("INDEX", "1") != "0",
        }
    }
}

/// What the indexer and the web server share.
#[derive(Clone)]
pub struct Shared {
    pub store: Store,
    pub state: Arc<RwLock<State>>,
    /// Pools that traded in each newly indexed block range.
    pub live: broadcast::Sender<Arc<Vec<u32>>>,
}

pub struct App {
    pub cfg: Arc<Config>,
    pub sh: Shared,
    pub css: String,
    pub js_file: String,
    pub js_url: String,
    pub wasm_file: String,
    pub wasm_url: String,
    pub lists: http::Lists,
    pub search: RwLock<Vec<(String, String, u32)>>,
    pub og: Mutex<OgCache>,
}

/// Rendered share images by (pool, Farcaster size) with the minute they were drawn.
type OgCache = HashMap<(u32, bool), (u32, Arc<Vec<u8>>)>;

/// FNV-1a, for cache-busting asset names.
fn hash(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{:08x}", h as u32)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Arc::new(Config::from_env());
    let store = Store::open(&cfg.data_dir)?;
    let started = std::time::Instant::now();
    let state = State::load(&store)?;
    eprintln!("loaded {} pools, {} tokens in {:.1?}", state.pools.len(), state.tokens.len(), started.elapsed());
    let sh = Shared { store, state: Arc::new(RwLock::new(state)), live: broadcast::channel(1024).0 };
    let (js_file, wasm_file) = (format!("app.{}.js", hash(JS.as_bytes())), format!("chart.{}.wasm", hash(WASM)));
    let app = Arc::new(App {
        css: pages::page_css(),
        js_url: format!("/a/{js_file}"),
        wasm_url: format!("/a/{wasm_file}"),
        js_file,
        wasm_file,
        cfg: cfg.clone(),
        sh: sh.clone(),
        lists: Default::default(),
        search: Default::default(),
        og: Default::default(),
    });
    if cfg.index {
        tokio::spawn(index::run(cfg.clone(), sh.clone()));
    }
    tokio::spawn(http::search_index(app.clone()));
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    eprintln!("listening on {}", cfg.listen);
    axum::serve(listener, http::router(app))
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        })
        .await?;
    sh.store.db.persist(fjall::PersistMode::SyncAll)?;
    Ok(())
}
