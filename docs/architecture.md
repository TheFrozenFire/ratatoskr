# Architecture

Ratatoskr's output is a normalized catalog of assets and venues on EVM chains. Two processes fill that catalog. They do not share a chain database.

## Discovery

Discovery names things. It reads logs and emits identities:

- an asset hint: chain, contract address, and a standard guess (`erc20`, `erc721`, `erc1155`, or `unclassified`)
- a venue hint: chain, kind, and either a contract address or a pool id

The standard guess comes from the log shape. `Transfer(address,address,uint256)` with two indexed addresses is treated as ERC-20. The same signature with three indexed words is treated as ERC-721. ERC-1155 uses `TransferSingle` and `TransferBatch`. A Uniswap V2 `PairCreated`, V3 `PoolCreated`, or V4 `Initialize` emits a venue plus unclassified asset hints for the currencies named in the event.

These are hints. The first log is not a deployment proof, and a matching signature is not a bytecode proof. Factory allowlists are a later filter on top of the same events.

Discovery does not read storage, balances, reserves, or ticks.

## State synchronization

State synchronization starts from identities discovery already produced. It watches later logs and reads the on-chain fields those venues need, such as AMM tick data. That process is not part of the first spike.

## Transports

Discovery only needs a stream of logs for a block range. The log record is transport-neutral: block number, block hash, transaction index, optional transaction hash, log index, address, topics, and data.

Planned sources, in order:

1. Ethereum mainnet EL devp2p, one outbound session. This is the spike.
2. The same session shape against other EVM chains whose peers serve receipts.
3. Portal Network history, where EL peers do not serve old receipts.
4. Static history files (era1 and similar) as a backfill when p2p history thins out.

SHiNode shows that an Ethereum peer will answer `GetBlockHeaders` and `GetReceipts` without the caller executing blocks. Ratatoskr keeps that lesson and drops the rest of SHiNode: no shard store, no JSON-RPC server, no follower, no embedded network node.

## Spike sequence

The spike is a short-lived client. It is done when one mainnet block's logs have been pulled over devp2p and run through the same classifier the unit tests use.

1. With no `--enode`, read the mainnet EIP-1459 DNS tree (`all.mainnet.ethdisco.net`) for peers that publish a TCP port. A discv4 lookup against the bootnodes runs beside that. This host is behind NAT, so bootnodes cannot complete the endpoint proof and that lookup returns nothing. An explicit `--enode` skips the search.
2. Generate an ephemeral secp256k1 key for each dial. It identifies that socket and is discarded on exit. Listen port in Hello is `0`. Nothing is bound for RLPx.
3. RLPx handshake via `reth-ecies`. Hello offers eth/68 through eth/72. Status uses mainnet genesis, the fork id for the current time, and a recent header read from a public JSON-RPC endpoint so the advertised head is a real block. Nothing is imported, stored, or served.
4. Negotiate the highest eth version both hellos advertised.
5. `GetBlockHeaders` for the peer's head minus 32, unless `--block` is set. Hash that header locally.
6. `GetReceipts` for that hash. eth/68 unwraps bloom-wrapped receipts. eth/69 returns bare receipts. eth/70 and later may split the block; the client continues from `firstBlockReceiptIndex` until the peer marks the block complete.
7. Flatten receipt logs into `NormalizedLog`. Transaction hashes are omitted. Discovery identities do not need them, and the body request can wait.
8. Run `ratatoskr-discover` on each log. Print one JSON object per hit, then a one-line count.
9. Drop the socket and exit. A peer that answers `too many peers` is skipped. The process dials several peers at once until one serves the block.

A peer that disconnects on the status fork check, or that has pruned the block, is skipped. The process still does not keep a peer set or a database.

## Corpus

`ratatoskr-ingest` is a separate process. It asks `ratatoskr-peer` for batches of up to 32 consecutive blocks, the same chunk size SHiNode uses. The peer returns headers and receipts. A block is stored only when the headers chain by parent hash and the receipt root matches the header. Empty receipt lists are kept only for blocks with an empty transaction root.

The redb file is `~/.local/share/ratatoskr/corpus.redb`. Table `blocks` keys a block number to a `BlockRecord`: header identity fields plus a zstd frame of EIP-2718 receipts. The default stop is 10GiB. Named ranges are the Uniswap v3 factory deployment week (block 12369621, capped at 2GiB), the Uniswap v4 PoolManager deployment (block 21688329, about 100k blocks, capped at 3GiB), and up to 400k recent blocks. eth/69 peers advertise `earliest`. A range that every live peer places before that block is not requested. A range that still fails with no header is left for a later run without punching holes through it. The peer table aims for 16 sessions. Each session can have two batches in flight, and ingest keeps up to 32 batches outstanding.

`ratatoskr-univ3` walks those receipts for Uniswap v3 `PoolCreated` logs from `0x1F98431c8aD98523631AE4a59f267346ea31F984`. The catalog stores the pool address, the two tokens, the fee, and the tick spacing. Those fields identify the pool for a later `ticks` read. The scan does not read pool storage.

`ratatoskr-headers` stores the header chain in `~/.local/share/ratatoskr/headers.redb`, table `headers`. A row keeps the hash, parent hash, timestamp, receipts root, and logs bloom. `GetBlockHeaders` is the only peer request. Block 0 has to be mainnet genesis, and each later header has to name the previous hash. The cursor is `meta.next`. The process stays 32 blocks behind the peer's head. Proof of work is not checked, so a peer that can extend a header whose parent is the real previous hash can still present a side chain. The bloom is what a later pass uses to decide which blocks are worth a receipt request.

## What stays out of the spike

- range sync, concurrency, and resume
- receipt-root checks against the header
- a kept peer set, or a listener that serves history
- Portal, OP-stack chains, and era files
- state reads of any kind
