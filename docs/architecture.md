# Architecture

By default one `ratatoskr` process owns both file sets (`--services db,peer`). `ratatoskr-db` and `ratatoskr-peer` are the split. The router is a library inside `ratatoskr`, not a third process and not a file owner.

## File owners

The database side owns `headers.redb`, `corpus.redb`, `discovery.redb`, `entities.redb`, and `work.redb`. That is `ratatoskr` with `--services db`, or `ratatoskr-db`. A path ending in `venues.redb` still names that directory. On open, a lone `venues.redb` is renamed to `entities.redb` and its crawl tables move into `discovery.redb`. An `entities.redb` that is already present leaves a sibling `venues.redb` closed.

The peer side owns `peers.redb` and is the only dialer. That is `ratatoskr` with `--services peer`, or `ratatoskr-peer`.

## Flock

`Database::open` takes an exclusive lock. A second open fails with "Database already open." Header sync and the venue walk cannot share a file with a second process. redb commits through two header slots. Durability is `Immediate` on the writes that have to survive power loss. There is no second journal.

`peers.redb` stays on the dialer so a peer-only process does not need the entity files. The default supervisor opens it fail-fast before the entity files.

## Sockets

Sockets are for other processes. Colocated pipelines call `LocalStore` and `LocalPeer` in process.

`ratatoskr-headers`, `ratatoskr-ingest`, and `ratatoskr-venues` are remote clients of `db.sock` and `peer.sock`.

`ratatoskr-univ3` reads `UNI_V3` rows through that socket. It does not open `corpus.redb` and it does not write `univ3-pools.redb`. That old file is left on disk, unopened.

## Cursor

Header sync and the receipt walk both await `Sink::push`. `HeaderSource` moves its cursor only when the sink returns `Applied`. A rejected header push retries the same cursor. A rejected receipt push fails the run, and the supervisor stops the pipelines.

Venue job cursors move inside `commit_chunk`. `VenueSource` does not await `Sink::push`.

`write_hole` does not move a cursor. `put_meta` does.

## Pipelines

Default pipelines are header sync and venue enumeration. The 10 GiB receipt walk is `--pipeline receipts` and is off by default. Pool state is `--pipeline state` and is off by default. It is not part of `RunPhase::All`. Spike is not started by the supervisor.

## Venue rows

Version 1 bytes stay valid. Version 2 appends `block_hash`, `tx_index`, and `log_index` when `FLAG_OBS` is set. List rows stay version 1. History rows from `rows_from_log` carry the observation. Merge keeps a stored observation when the new row has none.

## Non-goals

- state sync only when `--pipeline state` is named
- no Portal
- no serving devp2p (outbound Hello port 0 only)
- no second dialer
- no SQL
- no `eth_getLogs` backfill
