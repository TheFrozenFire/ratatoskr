# Ratatoskr

Ratatoskr enumerates DeFi assets and venues from historical EVM logs.

Discovery walks event streams and records pool identities. The state pipeline reads the quote inputs for identities discovery already found and stores a price curve. This repository does not run a node.

SHiNode is the reference for fetching Ethereum receipts over devp2p. Ratatoskr does not fork it. The peer service keeps outbound Ethereum execution-layer sessions. Portal Network and other EVM devp2p networks come after that session works.

## One process

`ratatoskr` owns the files and runs every pipeline in one process. The default is the database service and the peer service, with header sync, discovery, and the state pipeline.

```
cargo run -p ratatoskr
cargo run -p ratatoskr -- --data-dir data/ratatoskr --services db --pipeline state --until-idle
```

The peer service is the only dialer. It answers on `peer.sock`. The node key is `~/.config/ratatoskr/node.key`. With no `--data-dir`, the socket is `~/.config/ratatoskr/peer.sock`.

```
cargo run -p ratatoskr -- peer status
cargo run -p ratatoskr -- peer fetch
cargo run -p ratatoskr -- peer census
```

`peer fetch` reads one block from a session the peer service already holds and prints the discovery hits. `peer census` records the published mainnet DNS peer lists. Neither dials, and neither opens `peers.redb`.

Discovery writes pool identities into the data directory. Identities live in `entities.redb`. List cursors, bloom hits, and era1 receipt bodies live in `discovery.redb`. `work.redb` is only leases and retries. Factories that expose an index are read with a lens: `eth_call` of the creation code in `contracts/lens/IndexLens.sol`, with no `to`. The constructor reverts the rows, so any mainnet client that runs a creation call can serve it. The state pipeline opens the live catalog when `--data-dir` is omitted. `--copy-from` copies a stratified identity sample into that directory first. The detail lens reads a batch of pools. The tick lens reads initialized tick words in a second call. Fit stores a price curve from local quotes of those quote inputs. The next stage stores a pair conduit for each ordered pair, the slope merge of the direct actions between those tokens. A basket counts how many fixed-weight outputs those holdings still support. A pool used by more than one conduit is quoted from one trading set: same-direction tenders are one submission against the original reserves.

The lenses are a Foundry project. `foundry.toml` pins solc 0.8.28, the optimizer at 200 runs, Cancun, and legacy codegen. `forge test` checks the creation-code size and the revert magic. The hex file beside each lens is what the Rust client sends. Factories with no index are found by testing logs blooms in the local header chain. The hit list is kept in `discovery.redb`. Receipts for those hits come from era1 range requests before the merge and from the peer service after it, and the downloaded receipt bytes are kept in redb so a resume does not fetch them again. The local receipt corpus is folded in when it is present.

```
cargo run -p ratatoskr --release -- --pipeline venues
```

Header sync asks the peer service for header ranges and appends them. Each row is the block hash, parent hash, receipts root, and logs bloom. The chain has to link back to mainnet genesis. A restart continues from the saved cursor. Receipts are not downloaded.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the ingestion pipeline, the file owners, and the cursor rule. See [docs/venues.md](docs/venues.md) for the mainnet venues in use and how each set of pools is listed.
