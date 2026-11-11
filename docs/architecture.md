# Processes and files

The ingestion pipeline, from discovery through fit, is described in [ARCHITECTURE.md](../ARCHITECTURE.md). This file is the process and file layout.

One `ratatoskr` process owns both file sets (`--services db,peer` by default). The router is a library inside that process, not a file owner.

## File owners

The database service owns `headers.redb`, `corpus.redb`, `discovery.redb`, `entities.redb`, and `work.redb`. That is `ratatoskr` with `--services db`. A path ending in `venues.redb` still names that directory. On open, a lone `venues.redb` is renamed to `entities.redb` and its crawl tables move into `discovery.redb`. An `entities.redb` that is already present leaves a sibling `venues.redb` closed.

The peer service owns `peers.redb` and is the only dialer. That is `ratatoskr` with `--services peer`.

## Flock

`Database::open` takes an exclusive lock. A second open fails with "Database already open." Header sync and the venue walk cannot share a file with a second process. redb commits through two header slots. Durability is `Immediate` on the writes that have to survive power loss. There is no second journal.

`peers.redb` stays on the dialer so a peer-only process does not need the entity files. The default supervisor opens it fail-fast before the entity files.

## Sockets

Colocated pipelines call `LocalStore` and `LocalPeer` in process. The sockets stay so a second invocation can ask the running service without opening the files.

`ratatoskr peer status`, `ratatoskr peer fetch`, and `ratatoskr peer census` ask the peer service. They do not dial and they do not open `peers.redb`.

## Cursor

Header sync and the receipt walk both await `Sink::push`. `HeaderSource` moves its cursor only when the sink returns `Applied`. A rejected header push retries the same cursor. A rejected receipt push fails the run, and the supervisor stops the pipelines.

Venue job cursors move inside `commit_chunk`. `VenueSource` does not await `Sink::push`.

`write_hole` does not move a cursor. `put_meta` does.

## Pipelines

The default pipelines are header sync, discovery, the receipt walk, and the state pipeline. The receipt walk stores peer receipt bundles for its named ranges and has no global byte cap. The state pipeline is a separate task from `RunPhase::All`. With no `--data-dir` it opens the live catalog. `--copy-from` copies a stratified identity sample into `--data-dir` first. The peer service is the only dialer.

## Venue rows

Version 1 bytes stay valid. Version 2 appends `block_hash`, `tx_index`, and `log_index` when `FLAG_OBS` is set. List rows stay version 1. History rows from `rows_from_log` carry the observation. Merge keeps a stored observation when the new row has none.

## Non-goals

- no Portal
- no serving devp2p (outbound Hello port 0 only)
- no second dialer
- no SQL
- no `eth_getLogs` backfill
