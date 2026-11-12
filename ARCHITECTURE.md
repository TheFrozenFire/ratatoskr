# Architecture

Ratatoskr finds mainnet pools, reads the on-chain fields those pools use to quote, and stores a price curve for each one. The stored history is the header chain, the era1 receipt bytes of a pre-merge bloom hit, and the receipt bundle of a post-merge bloom hit. The stored pool data is the quote inputs and the price curve.

The entity flow is discovery, then a detail read, then the tick book where a pool has one, then fit. The joined inputs a local quote reads are the quote inputs: the configuration, the quoting state, and the tick book.

## Pipelines

Three pipelines run in one process. A bare `ratatoskr` starts the database service, the peer service, and all three: header sync, discovery, and the state pipeline. `--pipeline` selects a subset. The state pipeline is a separate task from `RunPhase::All`.

**Header sync** appends the header chain from peers: hash, parent, timestamp, receipts root, and logs bloom. It awaits `Sink::push`. The cursor advances when the sink returns `Applied`. A rejected push retries the same cursor.

**Discovery** writes pool identities. A factory that keeps a list is read by the index lens. A factory whose pools are named by a creation event is found by a bloom hit, then by that event in the receipt bundle. A registry covers Curve pools whose identity comes from the registry. The corpus can contribute creation events it already holds. A pool identity is a factory pool, a singleton pool, or a registry pool. A settlement records fills.

Discovery commits through the store. List cursors and bloom cursors move inside `commit_chunk`.

History for a bloom hit comes from an era1 file before the merge and from a peer after it. Pre-merge receipt bytes for that hit stay in the discovery store. A post-merge bloom hit reads its receipt bundle from the corpus when the bundle is already stored, and otherwise from a peer, then stores that bundle in the corpus. The creation events become pool identities in the same pass. An unserved span leaves the cursor on the last committed block. The stored history for a block is its header and, for a post-merge bloom hit, its receipt bundle.

**State pipeline** runs on identities discovery has already written. For each pool it stores configuration, quoting state, the tick book, and a price curve, then a pair conduit for each ordered pair those curves price. The work queue leases one stage at a time. The stages are configuration, quoting state, tick book, fit, and the pair conduit. With no `--data-dir` it opens the live catalog. `--copy-from` copies a stratified identity sample into `--data-dir` first.

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

Fit reads local quotes of the quote inputs. Chain quotes already stored stay in the quote record, for checking a price curve later. The live stages are configuration, quoting state, the tick book, fit, and the pair conduit. `venues::pause_work` in `crates/venues/src/lib.rs` stops those stages from claiming more work, and header and discovery reads wait with them. Peer sessions stay connected. `venues::resume_work` lets the same stages claim again. SIGUSR1 pauses and SIGUSR2 resumes. The status view's `p` key sends `POST /pause` or `POST /resume` to the exporter in that same process. A new quoting state drops the price curve and leaves the quote record in place until the pool identity is deleted.

An estimate is read from the stored price curve. A local quote is read from the quote inputs.

## Pair conduit

A pair conduit is the quote of every direct action that spends one token and pays another. At input `x` its value is the most those actions can pay together:

```
F(x) = max { sum f_i(x_i) | x_i >= 0, sum x_i <= x }
```

Each action is one direction of one pool. The stored row is the slope merge of those actions: the next input goes to the action whose marginal is currently highest. The two directions of one pool are members of opposite conduits. A path through a third token is a composition of pair conduits.

## Hub label

A hub label is the directed quote from a token to one hub. The opposite direction is a different label. When both labels name the same hub, the candidate is `hop_num / hop_den` in `crates/routing/src/hub.rs`. When the labels name different hubs, the candidate inserts the quote from the first hub to the second, `two_hub_num / two_hub_den`. The direct quote stays in the candidate set. Each candidate enters as its numerator times the other quote's denominator, and the choice is the maximum of those keys. Which hub wins can change with the input. A quote through a token that is not one of those hubs can beat both the direct quote and the hub quote.

The stage runs after fit. A curve write queues the ordered pairs of its priced directions. The first pass of a state run also queues pairs whose stored conduit is missing or behind the curves already in the catalog. The merge reads slope segments from `estimate` at the origin, the sampling ceiling, and the grid fractions of that ceiling. It does not hold a curve lease while it merges.

## Searcher

`crates/searcher` builds a route set from a trade. The trade names input tokens with amounts and the output tokens. The query adds the hub list and the wrapped token. Execution asks the caller for each pair conduit it needs and returns the direct curve, the one-hub routes, and the two-hub routes. A curve is stored once, and the slope segments keep the pool each piece spends. Native ETH is the zero address. The wrap to the wrapped token pays one wrapped token per native token at any size, and the reverse pays one native token per wrapped token. The catalog read stays `Catalog::read_conduit` and `Catalog::visit_conduits` in `crates/venues/src/conduit.rs`. The route set is what an optimizer allocates. The searcher does not allocate it. `bins/seek` opens the entity catalog, chooses trades from the stored conduits, and runs that search. The trades cover each venue kind, native ETH and the wrapped token, the hub pairs, one-hub leaf pairs, and token pairs whose shortest conduit path is longer than two hubs.

## Basket

A basket asks for output tokens in fixed weights. Holding `h` supports count `q` at weight `r` when `q * r <= h`. For a positive weight the last count is the floor quotient `h / r`. The basket count is the minimum of those last counts. Opening a pool is worth the baskets it adds, minus the cost of opening it.

## Depletion

Depletion is the reserve a quote uses. A pool named by more than one pair conduit shares one ledger. The trading set keeps one tender per direction and quotes it from the original reserves: `kept / denominator` of the tender enters the reserve, and same-direction tenders are one submission. The accrued-fee update deposits the whole tender and quotes the next piece against the new reserve. That update is not the ledger. Two directions of one multi-coin pool leave different reserves when their order is swapped.

On two roads that carry a demand of 2, one latency equals its flow and the other latency is the constant 2. The cost is 3 only when the flow on the linear road is 1. That split equalizes the linear road's marginal social cost with the constant latency. A cross link of latency 0 raises the even split from 3 to 4.

## Refresh

The watch reads receipt logs the peer service already holds and marks a pool dirty when a log is newer than its quoting state. Dirty work is claimed ahead of a quiet pool.

A swap or a sync is a head class: reserves or price moved. A mint or a burn is a tick class: liquidity positions changed. Tick class replaces head class. A head class with the same tick bitmap keeps the tick book. A tick class, or a changed bitmap, reads tick words again. Either way the new generation drops the price curve.

The dirty mark clears in the same write as the quoting state when the newest watched log is at or before that block.

## Processes and files

One `ratatoskr` process owns both file sets (`--services db,peer` by default). The peer service is the only dialer.

The database service owns `headers.redb` (the header chain), `corpus.redb` (the corpus), `discovery.redb` (the discovery store), `entities.redb` (the venue catalog), and `work.redb` (the work queue). That is `ratatoskr` with `--services db`. A path ending in `venues.redb` still names that directory. On open, a lone `venues.redb` is renamed to `entities.redb` and its crawl tables move into `discovery.redb`. An `entities.redb` that is already present leaves a sibling `venues.redb` closed.

The peer service owns `peers.redb`, the peer directory. That is `ratatoskr` with `--services peer`. A peer-only run opens the peer directory. The default supervisor opens that file before the entity files and stops if the open fails.

The venue catalog holds identities, configuration, quoting state, tick books, price curves, pair conduits, quote records, and watch marks. The discovery store holds list cursors, bloom hits, and era1 receipt bytes. The work queue holds leases. Deleting the work queue drops the leases. The pools remain in the venue catalog.

The state pipeline opens the data directory it is given. A sample catalog is a copy of identities for a trial. The live catalog is the venue catalog of the full identity set.

A pool identity with no observation stays version 1. An observation appends the block hash, transaction index, and log index and uses version 2. An identity read from a list has no observation. An identity read from a creation event carries the observation. A merge keeps a stored observation when the new identity has none.

### Flock

`Database::open` takes an exclusive lock. A second open fails with "Database already open." Header sync, discovery, and the state pipeline cannot share a file with a second process. redb commits through two header slots. Header append uses eventual durability. Venue commits, hit commits, era1 receipt bytes, state writes, corpus bundle writes, and cursor writes use immediate durability.

`peers.redb` stays with the dialer. A peer-only process leaves the entity files closed.

### Sockets

A colocated pipeline calls the store and the peer service in process. The sockets stay so a client can ask the running service. A client uses the database socket or the peer socket.

`ratatoskr peer status`, `ratatoskr peer fetch`, and `ratatoskr peer census` ask the peer service over its socket.

## Non-goals

- no Portal
- no serving devp2p (outbound Hello port 0 only)
- no second dialer
- no SQL
- no `eth_getLogs` backfill
