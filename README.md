# plainscreener

A fast, open-source DEX screener for Base: [plainscreener.com](https://plainscreener.com).

- **Every pool, straight from the chain.** Uniswap v2, v3 and v4, Aerodrome (classic and Slipstream), PancakeSwap v3 and every fork of them, indexed from Base logs. No third-party price API.
- **Light pages.** Server-rendered HTML with inline CSS and the chart drawn as SVG on the server, so the page paints before any script runs. About 3 KB of JS plus a ~14 KB wasm chart.
- **One chart library, three places.** `chart/` is a small Rust crate that renders candlesticks to SVG. The server inlines it in pages and rasterizes it for share images; browsers run the same code as wasm for panning, zooming and the crosshair.
- **Embeddable.** Every pair has `/embed/base/<pair>` for an iframe, plus Open Graph, X and Farcaster mini app cards with a live chart image.

## How it works

One Rust binary does everything:

1. **Indexer** (`server/src/index.rs`). It replays history from the free [SQD portal](https://docs.sqd.dev), takes the last ~1M blocks from a Base node (pruned nodes keep about that much), then follows the node block by block. Every source produces the same logs, which go through the same code path:
   - Pools are discovered from factory events emitted by any contract.
   - Swaps update prices, balances, candles (1m to 1d) and 24 h stats.
   - USD prices come from on-chain stablecoin pairs. Only pools from known factories (or Uniswap v4) can set a token's price, and only once they hold $1k of quote-side liquidity. A contract that emits fake pool events can't move prices or reach the lists. A pool's quote side is never valued above the real liquidity behind the quote token's own price, so a junk token priced at billions can't make its pools look deep.
2. **Store** (`server/src/store.rs`). An embedded [fjall](https://github.com/fjall-rs/fjall) LSM database holding tokens, pools, candles and trades. No separate database server.
3. **Web** (`server/src/http.rs`, `pages.rs`, `og.rs`). Pages are rendered per request in microseconds and cached by Cloudflare for a few seconds. Live prices, candles and trades stream over server-sent events.

## Run it

```sh
make wasm            # builds chart/ to web/chart.wasm (needs the wasm32-unknown-unknown target)
RPC_URL=http://your-base-node:8545 cargo run --release -p plainscreener
```

Settings (environment variables):

| Variable | Default | Meaning |
|---|---|---|
| `RPC_URL` | `http://127.0.0.1:8545` | Base node JSON-RPC, used for live blocks and token metadata |
| `SQD_URL` | `https://portal.sqd.dev/datasets/base-mainnet` | Where history older than the node's ~1M blocks comes from |
| `DATA_DIR` | `data` | Where the database lives |
| `START_BLOCK` | `0` | First block to index on an empty database |
| `LISTEN` | `0.0.0.0:8080` | HTTP address |
| `PUBLIC_URL` | `https://plainscreener.com` | Used in canonical links, share cards and embeds |
| `INDEX` | `1` | `0` serves the existing data without indexing |
| `OFFCHAIN` | `1` | `0` skips fetching icons, profiles and safety checks |

`plainscreener copy-offchain FROM` copies those three from the database in `FROM` into `DATA_DIR`, so a reindexed database can replace an old one without fetching them again.

Or use Docker: `docker build -t plainscreener .`. `deploy/` holds the production setup (compose next to a Base node, a Cloudflare tunnel, and a timer that deploys `main`).

## License

MIT
