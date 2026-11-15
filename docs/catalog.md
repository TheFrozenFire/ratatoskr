# Catalog access

Who may open the venue catalog. The file owners, the flock, and the cursor rule are in [ARCHITECTURE.md](../ARCHITECTURE.md). This page adds the two opens, what a reader can see, and which command takes the writer.

## Two opens

`venues::catalog::Catalog::open` in `crates/venues/src/catalog.rs` is a writer of `discovery.redb`, `entities.redb`, and `work.redb`. It opens each file in redb single-writer mode. A second writer fails.

`venues::catalog::EntityRead::open` opens `entities.redb` read-only in that same mode and follows the writer's commits. Discovery and the work queue stay closed. A reader must use the same concurrency mode as the writer. In this mode a read-only open shares the file with the writer, including another open in the same process. Dropping the writer first is not required for `EntityRead`. Do not open a second writer.

## What a reader can see

`EntityRead` reads token records, venue identity rows, and stored pair conduits (`tokens`, `read_venue`, and `read_conduit` / `visit_conduits` in `crates/venues/src/conduit.rs`). It does not read quoting state, tick books, or price curves. It has no `read_state`.

A check that needs quote inputs uses the writer: `venues::catalog::Catalog::read_state` in `crates/venues/src/state/mod.rs`, and only when no other writer holds the file. Do not open the live catalog with `Catalog::open` while the `ratatoskr` process is the writer.

## Commands

`seek` calls `survey::run` in `crates/survey/src/lib.rs`, which opens the catalog as a writer. `simulate` calls `survey::measure`, which opens `entities.redb` read-only. The read lock allows more than one reader. Run one `simulate` at a time so two allocations do not share the machine. Do not rebuild `target/release/ratatoskr` while that binary is the running writer. A measurement build is the `simulate` binary.

`--until-idle` stops the state pipeline after the queue is quiet: two passes that claim nothing. It is not the tip-following process. Header sync plus the peer service are what let the watch see new blocks.

The status view is `ratatoskr --status`. It polls the exporter at `127.0.0.1:9737` and draws header sync, discovery, the token lens, the state pipeline, feeds, spots, the peer directory, and the corpus. `q` closes only that view. Do not kill tmux session `ratatoskr-status` or the foot client of class `ratatoskr-status`. SIGTERM of `ratatoskr --status` skips terminal restore: the view restores the terminal only after its event loop returns, and it installs no signal handler.

Pause of stage claims is `venues::pause_work` in `crates/venues/src/lib.rs`. See [ARCHITECTURE.md](../ARCHITECTURE.md).

The live peer binds its tunnel addresses so session traffic, including the receipts discovery reads, leaves by that path. `ALCHEMY_API_KEY` is read from the environment or the repo `.env` and must not be printed.

## Prices and gas

The searcher and the optimizer quote in the tokens' own units. A price is applied after a plan exists, to compare input value with output value and to report gas. Do not add prices to search or to optimization. See [ARCHITECTURE.md](../ARCHITECTURE.md) Simulation.

The objective is the basket count. Gas is charged once on an action the first time one of its chords receives input. A gas ceiling can drop a step. Equal counts do not currently prefer less gas. Bringing that preference back is a same-count tie-break only. Do not subtract gas from the count.

`optimizer::solve::Candidate::beats` in `crates/optimizer/src/solve.rs` ranks a free slope, then a steeper slope, then a hub, then a larger delivery, then an earlier route. It does not read gas. The allocator does not call `routing::basket::open_count_wins` in `crates/routing/src/basket.rs`. The Basket sentence in [ARCHITECTURE.md](../ARCHITECTURE.md) treats opening cost as baskets added minus the cost of opening. That comparison is not what the allocator ranks by. The allocator objective remains the count.

## Other sessions

- A pricing pipeline measures after the plan. It does not enter the searcher or the optimizer, and it does not change stored curves.
- Diagnostic tooling reads through `EntityRead` ([docs/quotes.md](quotes.md)). It is not a searcher service and not a database API. redb has no replica log.
- Vegvisir extraction: `crates/searcher`, `crates/trade`, and `crates/optimizer` do not depend on `venues`. Catalog reads stay in `venues`. Vegvisir's own text does not name this repository. Do not add a dependency from those crates to the catalog as part of orientation. No change stream exists.

[PROBLEM.md](../PROBLEM.md) is the ingestion problem. [docs/search.md](search.md) is the search problem. [ARCHITECTURE.md](../ARCHITECTURE.md) is the pipeline. [docs/quotes.md](quotes.md) is how a quote is checked.
