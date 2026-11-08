# Ratatoskr

Ratatoskr enumerates DeFi assets and venues from historical EVM logs.

Discovery walks event streams and records identities. A later, separate process reads on-chain state (reserves, ticks, balances) for identities discovery already found. This repository does not sync chain state and does not run a node.

SHiNode is the reference for fetching Ethereum receipts over devp2p. Ratatoskr does not fork it. The first transport is one outbound Ethereum execution-layer session. Portal Network and other EVM devp2p networks come after that session works.

## Spike

`ratatoskr-spike` dials mainnet peers, reads one recent block of receipts, classifies the logs, prints the hits, and exits.

```
cargo run -p spike
cargo run -p spike -- --enode 'enode://<pubkey>@<ip>:<port>' --block 10000835
```

With no `--enode`, the process reads the mainnet DNS node list and dials peers that publish a TCP port. Most of them are full and disconnect. The process keeps dialing until one serves the block. With no `--block`, it reads 32 blocks behind that peer's reported head. The process listens on no port.

The offline half of the same path is `cargo test -p discover`. Those tests classify logs without a network.

`ratatoskr-peer` keeps a few outbound sessions open and answers on `~/.config/ratatoskr/peer.sock`. The node key is `~/.config/ratatoskr/node.key`.

```
cargo run -p peer
cargo run -p peer -- --fetch
cargo run -p peer -- --status
```

`ratatoskr-ingest` asks that process for receipt batches and writes them to `~/.local/share/ratatoskr/corpus.redb`. It walks the Uniswap v3 launch week (block 12369621) and the Uniswap v4 PoolManager launch (block 21688329) when a connected peer still serves those heights, then a recent window, and stops around 10GiB.

`ratatoskr-univ3` reads that corpus and writes pool identities to `~/.local/share/ratatoskr/univ3-pools.redb`. Each row is the pool address, token pair, fee, and tick spacing from `PoolCreated` on the canonical factory. Tick liquidity is not in the row. A later RPC call addresses the pool directly.

`ratatoskr-venues` writes every listed venue into `~/.local/share/ratatoskr/`. Identities live in `entities.redb`. List cursors, bloom hits, and era1 receipt bodies live in `discovery.redb`. `work.redb` is only leases and retries. Factories that expose an index are read with a lens: `eth_call` of the creation code in `contracts/lens/IndexLens.sol`, with no `to`. The constructor reverts the rows, so any mainnet client that runs a creation call can serve it. Pool state is a separate pass, `--pipeline state`, and stays off unless it is named. It requires `--data-dir` and does not open the live catalog. `--copy-from` copies a stratified identity sample into that directory first. `DetailLens` reads a batch of pools. `TickLens` reads initialized tick words in a second call. Curves are derived from those rows.

The lenses are a Foundry project. `foundry.toml` pins solc 0.8.28, the optimizer at 200 runs, Cancun, and legacy codegen. `forge test` checks the creation-code size and the revert magic. The hex file beside each lens is what the Rust client sends. Factories with no index are found by testing logs blooms in the local header chain. The hit list is kept in `discovery.redb`. Receipts for those hits come from era1 range requests before the merge and from `ratatoskr-peer` after it, and the downloaded receipt bytes are kept in redb so a resume does not fetch them again. The local receipt corpus is folded in when it is present.

```
cargo run -p venues-cli --release
```

`ratatoskr-headers` asks the peer process for header ranges. The database server appends them. This binary does not open `headers.redb`. Each row is the block hash, parent hash, receipts root, and logs bloom. The chain has to link back to mainnet genesis. A restart continues from the saved cursor. Receipts are not downloaded.

See [docs/architecture.md](docs/architecture.md) for the file owners and the cursor rule. See [docs/venues.md](docs/venues.md) for the mainnet venues in use and how each set of pools is listed.
