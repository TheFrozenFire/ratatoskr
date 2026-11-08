# Architecture

Two processes own files. The router is a library inside `ratatoskr`, not a third process and not a file owner.

## File owners

The database server owns `headers.redb`, `corpus.redb`, and `venues.redb`. That server is `ratatoskr` with `--services db`, or `ratatoskr-db`.

The peer service owns `peers.redb` and is the only dialer.

## Flock

`Database::open` takes an exclusive lock. A second open fails with "Database already open." Header sync and the venue walk cannot share a file with a second process.

`peers.redb` stays on the dialer so a peer-only process does not need the entity files. The default supervisor opens it fail-fast before the entity files.

## Sockets

Sockets are for other processes. Colocated pipelines call `LocalStore` and `LocalPeer` in process.

`ratatoskr-headers`, `ratatoskr-ingest`, and `ratatoskr-venues` are remote clients of `db.sock` and `peer.sock`.

`ratatoskr-univ3` reads `UNI_V3` rows through that socket. It does not open `corpus.redb` and it does not write `univ3-pools.redb`. That old file is left on disk, unopened.

## Cursor

`Source::run` awaits `Sink::push`. The cursor moves only when the sink returns `Applied`. A rejected push retries the same cursor. `write_hole` does not move a cursor. `put_meta` does.

## Pipelines

Default pipelines are header sync and venue enumeration. The 10 GiB receipt walk is `--pipeline receipts` and is off by default. Spike is not started by the supervisor.

## Venue rows

Version 1 bytes stay valid. Version 2 appends `block_hash`, `tx_index`, and `log_index` when `FLAG_OBS` is set. List rows stay version 1. History rows from `rows_from_log` carry the observation. Merge keeps a stored observation when the new row has none.

## Non-goals

- no state sync
- no Portal
- no serving devp2p (outbound Hello port 0 only)
- no second dialer
- no SQL
- no `eth_getLogs` backfill
