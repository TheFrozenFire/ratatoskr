# Architecture

Ratatoskr finds mainnet pools, reads the on-chain fields those pools use to quote, and stores a price curve for each one. The stored history is the header chain and the receipt bundles used to find those pools. The stored pool data is the quote inputs and the price curve.

The entity flow is discovery, then a detail read, then the tick book where a pool has one, then fit. The joined inputs a local quote reads are the quote inputs: the configuration, the quoting state, and the tick book.

## Pipelines

Four pipelines run under the database service. Header sync and discovery are the default. The receipt walk and the state pipeline run when named.

**Header sync** appends the header chain from peers: hash, parent, timestamp, receipts root, and logs bloom. The cursor advances when the append is applied.

**Discovery** writes pool identities. A factory that keeps a list is read by the index lens. A factory whose pools are named by a creation event is found by a bloom hit, then by that event in the receipt bundle. A registry covers Curve pools whose identity comes from the registry. The corpus can contribute creation events it already holds. A pool identity is a factory pool, a singleton pool, or a registry pool. A settlement records fills.

**Receipt walk** stores receipt bundles for named block ranges. Discovery reads creation events out of those bundles and writes the identities.

**State pipeline** runs on identities discovery has already written. For each pool it stores configuration, quoting state, the tick book, and a price curve. The work queue leases one stage at a time. The stages are configuration, quoting state, tick book, and fit.

History for a bloom hit comes from an era1 file before the merge and from a peer after it. A hole records an unserved block, and the cursor stays on the last committed block. The stored history for a block is its header and, when a receipt walk kept it, its receipt bundle.

## Detail read

The detail lens reads a batch of pools in one call. That call returns the configuration and the quoting state. Tick words are read later into the tick book.

**Configuration** is written once. It holds the pool's tokens, and the facts that identify how the pool is built: the home contract, a hook, tick spacing, and a concentrated pool's fee. A later read that disagrees is a mismatch. The stored configuration stays.

**Quoting state** is the block-pinned record of the values that change. It holds reserves, a constant-product pool's fee, price, in-range liquidity, the tick bitmap, protocol fee, balances, weights, amplification, rates, price scales, and a Metric pool's bins. Each successful write is a new generation.

The tick bitmap is part of the quoting state. It names which ticks are initialized. The liquidity at those ticks is the tick book.

An unpriced quoting state stays in the catalog on the quoting-state stage. A truncated book, whose tick bitmap was cut off, stays on the quoting-state stage until a later read returns the rest. A failed read records the pool.

## Tick book

The tick lens reads tick words for ticks the bitmap has named. The tick book is those words, stored for one generation. A page is one stored slice of the book.

A concentrated pool with initialized ticks needs its tick book before fit, because the ladder and the sampling ceiling depend on the tick words. A pool with an empty tick bitmap goes from quoting state to fit.

A head-class refresh with an unchanged bitmap keeps the tick pages and marks them with the new generation. Any other new quoting state replaces the tick book.

## Fit

Fit stores the price curve for one generation. A local quote is computed from the quote inputs. Selection may score formulas against a grid of those local quotes. The stored price curve is the formula selection keeps, or a skipped curve.

Fit reads local quotes of the quote inputs. Chain quotes already stored stay in the quote record, for checking a price curve later. The live stages are configuration, quoting state, the tick book, and fit. A new quoting state drops the price curve and leaves the quote record in place until the pool identity is deleted.

An estimate is read from the stored price curve. A local quote is read from the quote inputs.

## Refresh

The watch reads receipt logs the peer service already holds and marks a pool dirty when a log is newer than its quoting state. Dirty work is claimed ahead of a quiet pool.

A swap or a sync is a head class: reserves or price moved. A mint or a burn is a tick class: liquidity positions changed. Tick class replaces head class. A head class with the same tick bitmap keeps the tick book. A tick class, or a changed bitmap, reads tick words again. Either way the new generation drops the price curve.

The dirty mark clears in the same write as the quoting state when the newest watched log is at or before that block.

## Processes and files

One process owns both file sets by default. The database service owns the header chain, the corpus, the discovery store, the venue catalog, and the work queue. The peer service owns the peer directory and is the only dialer. A colocated pipeline calls the store in process. A client reaches the files through the database socket or the peer socket.

The venue catalog holds identities, configuration, quoting state, tick books, price curves, quote records, and watch marks. The discovery store holds list cursors, bloom hits, and era1 receipt bytes. The work queue holds leases. Deleting the work queue drops the leases. The pools remain in the venue catalog.

The database files take one exclusive open. A peer-only process opens the peer directory.

The state pipeline opens the data directory it is given. A sample catalog is a copy of identities for a trial. The live catalog is the venue catalog of the full identity set.

File ownership, the cursor rule, and the socket split are also recorded in [docs/architecture.md](docs/architecture.md).
