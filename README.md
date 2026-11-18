# plainscreener

A fast, open-source DEX screener for Base: [plainscreener.com](https://plainscreener.com).

- **Every pool, straight from the chain.** Uniswap v2, v3 and v4, Aerodrome (classic and Slipstream), PancakeSwap v3 and every fork of them, indexed from Base logs. No third-party price API.
- **Light pages.** Server-rendered HTML with inline CSS and the chart drawn as SVG on the server, so the page paints before any script runs. About 3 KB of JS plus a ~14 KB wasm chart.
- **One chart library, three places.** `chart/` is a small Rust crate that renders candlesticks to SVG. The server inlines it in pages and rasterizes it for share images; browsers run the same code as wasm for panning, zooming and the crosshair.
- **Embeddable.** Every pair has `/embed/base/<pair>` for an iframe, plus Open Graph, X and Farcaster mini app cards with a live chart image.

## How it works

One Rust binary does everything:

1. **Indexer** (`server/src/index.rs`). It backfills history from [HyperSync](https://envio.dev), then follows a Base node block by block. Both sources produce the same logs, which go through the same code path:
   - Pools are discovered from factory events emitted by any contract.
   - Swaps update prices, balances, candles (1m to 1d) and 24 h stats.
   - USD prices come from on-chain stablecoin pairs. A pool needs at least $1k of quote-side liquidity before it can set a token's price, which keeps dust pools from inventing prices.
2. **Store** (`server/src/store.rs`). An embedded [fjall](https://github.com/fjall-rs/fjall) LSM database holding tokens, pools, candles and trades. No separate database server.
3. **Web** (`server/src/http.rs`, `pages.rs`, `og.rs`). Pages are rendered per request in microseconds and cached by Cloudflare for a few seconds. Live prices, candles and trades stream over server-sent events.

## Run it

```sh
make wasm            # builds chart/ to web/chart.wasm (needs the wasm32-unknown-unknown target)
HYPERSYNC_TOKEN=… RPC_URL=http://your-base-node:8545 cargo run --release -p plainscreener
```

Settings (environment variables):

| Variable | Default | Meaning |
|---|---|---|
| `RPC_URL` | `http://127.0.0.1:8545` | Base node JSON-RPC, used for live blocks and token metadata |
| `HYPERSYNC_TOKEN` | — | Envio HyperSync token, used to backfill anything older than a few thousand blocks |
| `DATA_DIR` | `data` | Where the database lives |
| `START_BLOCK` | `0` | First block to index on an empty database |
| `LISTEN` | `0.0.0.0:8080` | HTTP address |
| `PUBLIC_URL` | `https://plainscreener.com` | Used in canonical links, share cards and embeds |
| `INDEX` | `1` | `0` serves the existing data without indexing |

Or use Docker: `docker build -t plainscreener .`. `deploy/` holds the production setup (compose next to a Base node, a Cloudflare tunnel, and a timer that deploys `main`).

## License

MIT
