# Ratatoskr

Ratatoskr is the catalog of mainnet pools and the history used to find them. The words below keep a pool, its configuration, its quoting state, its tick book, the stored price curve, a chain quote, a receipt, and a peer from being treated as the same thing. The ingestion flow is described in [ARCHITECTURE.md](ARCHITECTURE.md).

## Language

### Pools

**Pool**:
A venue that prices a swap between its tokens.
_Avoid_: pair, market, venue

**Pool identity**:
The durable name of one pool: its kind, its pool id, the factory or singleton it belongs to, and the tokens named when it was listed.
_Avoid_: row, address, venue

**Kind**:
The venue family on a pool identity, such as Uniswap v2, a v2 fork, Uniswap v3, Aero, Uniswap v4, Balancer v2, Balancer v3, Curve, or Metric.
_Avoid_: shape, protocol, type

**Shape**:
The quoting rule of a pool: constant product, concentrated, Balancer, Curve, or Metric.
_Avoid_: kind

**Factory**:
The contract that lists a kind or emits its creation events.
_Avoid_: router, vault, registry

**Singleton**:
One contract that holds many pools, each named by a pool id.
_Avoid_: factory, pool manager

**Pool id**:
The 32-byte name of a pool. A factory pool stores the pool address in the low bytes. A singleton pool stores the id from its creation event.
_Avoid_: address, key

**Token**:
An asset a pool prices.
_Avoid_: currency, coin, asset

**Hook**:
An external contract named in a singleton pool's identity, able to change that pool's fee or its swap.
_Avoid_: extension, callback

**Fork**:
A factory that emits Uniswap's creation event and is not the Uniswap factory.
_Avoid_: clone, unofficial pool

**Settlement**:
A venue that records fills and has no pool contract.
_Avoid_: auction, order book, solver

**Registry**:
A contract that lists Curve pools, including pools no factory event named.
_Avoid_: factory, address provider

**List**:
An on-chain array of pool ids, such as all pairs, all pools, a pool list, or an index-to-pool view.
_Avoid_: index, log, census

**Creation event**:
The log that names a new pool when the factory keeps no list.
_Avoid_: list, deploy, discovery hit

**Observation**:
The chain, block, transaction, log index, and emitter of one log.
_Avoid_: receipt, sighting

**Discovery hit**:
An asset or a pool identity read from the shape of one log.
_Avoid_: bloom hit, watch hit, creation event

**Asset**:
An ERC-20, ERC-721, or ERC-1155 named by a transfer-shaped log. The standard is a guess from the topic count.
_Avoid_: token, pool

**Canonical factory**:
The one emitter accepted for a kind that many contracts share, such as Uniswap v4, Balancer, or Metric.
_Avoid_: canonical pool

### Shapes

**Constant-product pool**:
A two-token pool whose price curve is determined by two reserves and a pool fee.
_Avoid_: pair, v2, xy=k

**Reserve**:
The token balance a constant-product pool prices from.
_Avoid_: balance, liquidity

**Concentrated pool**:
A pool whose liquidity sits between ticks. Uniswap v3, its forks, Aero, and Uniswap v4 have this shape.
_Avoid_: v3, range, CL pool

**Price**:
A concentrated pool's current square-root price and the tick that price sits on.
_Avoid_: spot, marginal price, quote

**Liquidity**:
The in-range liquidity of a concentrated pool.
_Avoid_: reserve, depth, tick word

**Tick**:
A price boundary where concentrated liquidity can be added or removed.
_Avoid_: tick word, bin, bitmap

**Tick word**:
The liquidity gross and the liquidity net at one initialized tick.
_Avoid_: tick, bitmap, slot

**Tick bitmap**:
The set of words that name which ticks are initialized.
_Avoid_: tick word, book, liquidity

**Balancer pool**:
A pool whose tokens and balances live in a shared vault. The pool is weighted or stable.
_Avoid_: vault, weighted product

**Weight**:
A Balancer token's static share of pool value.
_Avoid_: balance, amplification

**Curve pool**:
A pool of Curve coins. It is a stableswap or a crypto pool.
_Avoid_: price curve, curve

**Curve coin**:
One token of a Curve pool, with its balance, rate, and price scale.
_Avoid_: token, reserve

**Amplification**:
The stableswap parameter that holds price near one until balances diverge.
_Avoid_: gamma, weight

**Stableswap**:
A Curve pool whose price curve is determined by coin balances, amplification, rates, and a pool fee.
_Avoid_: crypto pool, stable Balancer pool

**Crypto pool**:
A Curve pool whose quoting state carries price scales and gamma.
_Avoid_: stableswap

**Metric pool**:
An oracle-bin pool whose current price sits in one bin.
_Avoid_: concentrated pool, oracle

**Bin**:
One price interval of a Metric pool, with scaled balances of both tokens.
_Avoid_: tick, bucket

**Pool fee**:
The fraction of input the pool withholds.
_Avoid_: protocol fee, gas, spread

**Protocol fee**:
A fee recorded on a concentrated pool's quoting state beside the pool fee.
_Avoid_: pool fee, hook

### Quote inputs

**Quote inputs**:
The configuration, the quoting state, and the tick book of one pool at one generation. A local quote is computed from the quote inputs.
_Avoid_: snapshot, state, detail, pool state

**Configuration**:
The pool facts written once: tokens, the home contract, a hook, tick spacing, and a concentrated pool's fee.
_Avoid_: quoting state, identity, details, immutable

**Quoting state**:
The block-pinned values a quote reads aside from tick words, such as reserves, price, in-range liquidity, the tick bitmap, balances, weights, and amplification.
_Avoid_: snapshot, sync, mutable, head, configuration

**Generation**:
The identity of one quoting state. The tick book and the price curve of that quoting state carry the same generation.
_Avoid_: version, block, cursor

**Tick book**:
The tick words stored for one generation.
_Avoid_: tick bitmap, sync, snapshot, quoting state

**Chain state**:
A node's execution state. This project does not store chain state.
_Avoid_: quoting state, quote inputs, header chain

**Mismatch**:
A later read of configuration that disagrees with the stored configuration. The stored configuration stays.
_Avoid_: dirty, failed read

**Tick page**:
One stored slice of a tick book.
_Avoid_: tick bitmap, tick lens, tick book

**Unpriced**:
A quoting state that cannot price a positive input. The pool identity stays, and there is no price curve.
_Avoid_: skipped curve, hole, failed read

**Truncated book**:
A quoting state whose tick bitmap was cut off before every initialized tick was named.
_Avoid_: unpriced, tick page

**Failed read**:
A detail read that records the pool and records that its fields did not come back.
_Avoid_: unpriced, mismatch

### Price curves

**Price curve**:
The stored relation from an input amount of one token to the output amount of another, for one direction of one pool at one generation.
_Avoid_: the function, fit, curve, formula, quote

**Direction**:
One ordered pair of tokens, input then output.
_Avoid_: side, leg, pair

**Formula**:
A named closed form that produces the output of a price curve on one direction.
_Avoid_: price curve, function, model

**Product formula**:
The formula of a constant-product pool.
_Avoid_: reserve, constant-product pool

**Ladder**:
The formula of a concentrated pool.
_Avoid_: tick walk, concentrated quote, v3 math

**Structural formula**:
A formula whose parameters are quote inputs, such as the product formula, the ladder, a stableswap, a weighted Balancer pool, a stable Balancer pool, or a bin.
_Avoid_: sample formula, canonical pool

**Sample formula**:
A formula drawn through knots on the grid, such as a chord, a cubic, a residual, or a wave.
_Avoid_: structural formula, interpolation, chain quote

**Knot**:
One input amount and the output amount a sample formula is drawn through.
_Avoid_: grid, holdout, label

**Grid**:
The series of quotes used to sample a price curve. Each point is a fraction of the sampling ceiling.
_Avoid_: knots, chain quotes, domain

**Sampling ceiling**:
The largest input on a direction that still has a price.
_Avoid_: max in, balance, reserve, domain

**Holdout**:
A grid point at one quarter or four fifths of the sampling ceiling, withheld from choosing a formula's parameters and used only to score the formula.
_Avoid_: knot, label, test set

**Label**:
The output amount a formula is scored against. A label names its source.
_Avoid_: chain quote, local quote

**Local quote**:
An output amount computed from the quote inputs.
_Avoid_: chain quote, estimate, label

**Chain quote**:
An output amount from the venue's own quoter at the quoting state's block.
_Avoid_: local quote, label, price curve

**Quote record**:
The stored chain quotes for one quoting state and its generation.
_Avoid_: price curve, local quote, grid

**Quoter**:
The venue contract that returns a chain quote.
_Avoid_: lens, router

**Estimate**:
The output amount a stored price curve assigns to an input on one direction.
_Avoid_: local quote, chain quote

**Pair conduit**:
The stored quote of every direct swap that spends one token and pays another, as the slope merge of those swaps.
_Avoid_: route, hop, action

**Slope segment**:
A positive input width and a nonnegative output on one pool direction. A zero-output segment is not stored.
_Avoid_: cap, hop, tick

**Meta-action**:
A stored edge built from pool directions. A pool direction is not a meta-action.
_Avoid_: route, pool direction, hop

**Action**:
A synchronous transform that spends tokens and pays tokens in one transaction. Amounts, fees, and reserves are not part of the action. The set is a custodial wrap, an exchange-rate wrap, a swap, a fungible liquidity mint or burn, a vault deposit or redeem, a rebasing supply or withdraw, a synchronous stake or unstake, a peg convert or a migration, an order fill, and a split or merge of principal and yield.
_Avoid_: slope segment, pair conduit, route

**Custodial wrap**:
A one-for-one claim on a token the wrapper holds. Native ETH and wrapped ETH are this wrap.
_Avoid_: vault deposit, liquidity mint

**Liquidity mint**:
Depositing a pool's tokens to receive a fungible share. The share is priced in the pool's ratio, from an unbalanced deposit, or from one token. Burning the share pays those tokens back. A concentrated-liquidity position NFT is not a share.
_Avoid_: vault deposit, swap

**Vault deposit**:
Paying an ERC-4626 vault's asset, or a cToken's underlying, to receive shares in the same transaction. Redeeming the shares pays the asset back. A queued request is not a vault deposit.
_Avoid_: liquidity mint, custodial wrap

**Order fill**:
A settlement contract paying tokens for one order. The order has no pool curve. A side may name several tokens.
_Avoid_: swap, pair conduit

**Basket**:
A demand for output tokens in fixed weights. The count is the largest integer every output still supports. Holding `h` supports count `q` at weight `r` when `q * r <= h`.
_Avoid_: route, pair conduit, hop

**Depletion**:
The reserve state a pool is quoted against.
_Avoid_: quoting state, generation

**Trading set**:
One tender and one receipt for a direction, taken from the original reserves. `kept / denominator` of the tender enters a constant-product reserve. Same-direction tenders are one submission.
_Avoid_: accrued fee, quote record

**Hub label**:
The directed quote from a token to one hub. The opposite direction is a different label. The same hub on both labels quotes `hop_num / hop_den`. Two hubs insert `two_hub_num / two_hub_den`. The direct quote stays among the candidates. The choice is the maximum of the cleared cross keys.
_Avoid_: pair conduit, route

**Route set**:
The curves a search returns for one trade. It carries the input amounts, the output tokens, and each used curve once. The routes are the direct pair, a path through one hub, a path through two hubs, and a bounded path from an endpoint that names few conduits.
_Avoid_: plan, basket

**Searcher**:
The build of a route set from a trade. It reads stored pair conduits, walks endpoints that name few conduits, and applies the wrap between native ETH and wrapped ETH. It does not allocate the flow.
_Avoid_: optimizer, state pipeline

**Trade**:
The inputs, the basket weights, the wrapped token, and the gas ceiling a caller stands behind. It builds the search and turns the route set into a problem of actions.
_Avoid_: route set, plan, request

**Optimizer**:
The allocation of input amounts across a problem of actions. It bisects the basket count and books one net input on each shared edge. It does not read the catalog.
_Avoid_: searcher, state pipeline

**Plan**:
The basket count a problem can fund, the input spent on each route, and the gas of the actions that receive input.
_Avoid_: route set, quote

**Acceptance bar**:
The worst relative error a stored formula is allowed on its labels: ten basis points.
_Avoid_: score, tolerance

**Skipped curve**:
A price curve that names no formula.
_Avoid_: unpriced, failed read

**Canonical pool**:
A pool whose structural formula is treated as determined by the quote inputs: a constant-product pool, a concentrated pool, an equal-weight or stable Balancer pool, or a stableswap.
_Avoid_: canonical factory

**Selection**:
The choice of one formula for a direction by scoring candidates against labels.
_Avoid_: fit, derive, estimate

### History

**Header**:
The stored identity of a block: hash, parent, timestamp, receipts root, and logs bloom.
_Avoid_: body, receipt, head

**Header chain**:
The checked sequence of headers from mainnet genesis, each header's parent being the previous hash.
_Avoid_: corpus, chain state

**Logs bloom**:
The header field that can prove a block has no log for an address and a topic.
_Avoid_: bloom hit, log filter

**Log filter**:
An address set and up to four topic positions. An empty position is a wildcard.
_Avoid_: creation event, discovery hit

**Bloom hit**:
A block whose logs bloom can contain a log filter.
_Avoid_: discovery hit, watch, proof

**Receipt**:
The result of one transaction, including its logs.
_Avoid_: header, block body

**Receipt bundle**:
The receipts of one block, checked against that block's receipts root.
_Avoid_: corpus, header

**Corpus**:
The stored receipt bundles. Discovery adds the bundle for a post-merge bloom hit.
_Avoid_: venue catalog, header chain, discovery store

**Block body**:
The transactions of a block. This project does not store block bodies.
_Avoid_: receipt bundle, era1 file

**Hole**:
A block recorded as unserved.
_Avoid_: unpriced, cursor

**Cursor**:
The next block or list index a walk has not committed.
_Avoid_: generation, head, lease

**Era1 file**:
A pre-merge history object holding a header, a block body, receipts, and an index for each block of one epoch.
_Avoid_: corpus, beacon era

**Merge**:
The boundary after which headers and receipts come from peers, and before which receipt history for a bloom hit comes from era1 files.
_Avoid_: genesis, hard fork

**Normalized log**:
One log with its block, transaction, address, topics, and data, independent of the transport that returned it.
_Avoid_: receipt, discovery hit

### Peers

**Peer**:
An Ethereum execution-layer node this project dials outbound. This project does not serve that protocol.
_Avoid_: peer service, bootnode

**Session**:
One outbound connection to a peer.
_Avoid_: sighting, dial

**History window**:
The earliest and latest block a peer has advertised or has served. A block announcement after the handshake raises the latest block.
_Avoid_: cursor, corpus

**Published node**:
A node record from a public DNS discovery tree, before this project has dialed it.
_Avoid_: summary, peer

**Sighting**:
One recorded contact with a peer: a status, a served block, an empty answer, a drop, a failed dial, or a full inbound table.
_Avoid_: summary, session

**Summary**:
The dial hint derived from a peer's sightings: last endpoint, last window, and the blocks it has served.
_Avoid_: sighting, published node

**Dial**:
One attempt to open a session.
_Avoid_: sighting, fetch

### Services and work

**Seed file**:
`corpus/mainnet.yaml`. It names the wrapper, the enumeration roots, and quote contracts the chain does not point at. Ingest writes list shapes and deploy blocks into the venue catalog, asks the address provider for registries on each discovery start, and the token lens writes symbols and decimals onto token records.
_Avoid_: corpus, receipt bundle

**Venue catalog**:
Pool identities, token records, configuration, quoting state, tick books, price curves, quote records, and watch marks.
_Avoid_: catalog, corpus, peer directory

**Discovery store**:
List cursors, bloom hits, and the era1 receipt bytes kept so a hit is not fetched twice.
_Avoid_: venue catalog, corpus

**Work queue**:
The leased jobs for pool state. Removing the queue drops leases, not pools.
_Avoid_: cursor, state pipeline

**Peer directory**:
Published nodes, sightings, and summaries.
_Avoid_: venue catalog, peer service

**Database service**:
The service that owns the header chain, the corpus, the discovery store, the venue catalog, and the work queue.
_Avoid_: peer service, client

**Peer service**:
The service that owns dialing and the peer directory, and the only dialer.
_Avoid_: peer, database service

**Colocated pipeline**:
A pipeline running inside the database service and reading the store in process.
_Avoid_: client

**Client**:
A process that uses the database socket or the peer socket and does not open the files.
_Avoid_: colocated pipeline, peer

**Lens**:
A read that sends contract creation code and takes the rows from the revert. Nothing is deployed.
_Avoid_: quoter, view

**Index lens**:
The lens that reads a slice of a list.
_Avoid_: creation event, detail lens

**Token lens**:
The lens that reads symbol and decimals for a batch of tokens.
_Avoid_: detail lens, index lens

**Detail lens**:
The lens that reads a batch of pools for configuration and quoting state, including tick bitmaps and not tick words.
_Avoid_: tick lens, quoter

**Tick lens**:
The lens that reads tick words for ticks a tick bitmap has named.
_Avoid_: detail lens, tick bitmap

**Header sync**:
The pipeline that appends headers.
_Avoid_: discovery, state pipeline

**Discovery**:
The pipeline that writes pool identities. It reads enumeration roots from the seed file, walks lists and bloom hits, folds receipt bundles the corpus already holds, and then reads token symbol and decimals through the token lens. It repeats as the header cursor advances, and it folds the corpus once in a process.
_Avoid_: state pipeline, discovery hit, enumeration, census

**State pipeline**:
The pipeline that writes configuration, quoting state, tick books, and price curves for pool identities discovery has already stored. Each pass reads at the latest stored header.
_Avoid_: discovery, chain state

**Watch**:
The scan of receipt logs the peer service already holds, used to mark a pool dirty.
_Avoid_: bloom hit, header sync

**Dirty**:
A mark that a pool's log is newer than its quoting state.
_Avoid_: mismatch, lease, skipped curve

**Head class**:
A dirty mark that reserves or price moved.
_Avoid_: head, quoting state, tick class

**Tick class**:
A dirty mark that liquidity positions changed.
_Avoid_: tick page, head class

**Fit**:
The stage that stores a price curve for one generation, computed from local quotes of the quote inputs.
_Avoid_: selection, snapshot, sync

**Stage**:
One leased step for a pool: configuration, quoting state, the tick book, or fit.
_Avoid_: quote stage, phase

**Lease**:
A claim on one work-queue job that other workers leave alone until it expires.
_Avoid_: cursor, lock

**Live catalog**:
The venue catalog of the full identity set.
_Avoid_: sample catalog

**Sample catalog**:
A smaller copy of pool identities, used for a trial.
_Avoid_: live catalog, quote record, grid

## Relationships

- A **Pool identity** has exactly one **Kind** and one **Pool id**, and it comes from one **Factory**, one **Singleton**, or one **Registry**
- A **Kind** is not a **Shape**. A v2 fork is a kind whose shape is a constant-product pool
- A **Settlement** is not a **Pool identity**
- A **Pool identity** has one **Configuration** and at most one current **Quoting state**
- A **Quoting state** has one **Generation**. Its **Tick book** and its **Price curve** carry that generation
- The **Quote inputs** are the **Configuration**, the **Quoting state**, and the **Tick book**
- A **Mismatch** does not replace stored **Configuration**
- An **Unpriced** quoting state keeps the **Pool identity** and has no **Price curve** and no **Tick book**
- **Tick words** belong to a **Tick book**. A **Tick bitmap** is part of the **Quoting state** and names which ticks exist
- A **Price curve** belongs to one **Generation**. Each **Direction** has one **Formula**, or the record is a **Skipped curve**
- A **Product formula** is determined by the two **Reserves** and the **Pool fee**
- A **Ladder** is determined by the **Price**, the in-range **Liquidity**, and the **Tick words**
- A **Hook** is part of a concentrated pool's **Configuration**. A **Protocol fee** sits on its **Quoting state**. Neither is a parameter of the **Ladder**
- A **Stableswap** price curve is determined by **Curve coin** balances, **Amplification**, rates, and the **Pool fee**
- A weighted **Balancer pool**'s price curve is determined by the two balances, the two **Weights**, and the **Pool fee**
- A stable **Balancer pool**'s price curve is determined by the balances, **Amplification**, and the **Pool fee**
- A **Metric pool**'s structural formula is its current **Bin**
- A **Crypto pool**'s **Local quote** is a spot line through its price scales, not the crypto invariant
- **Selection** scores formulas against **Labels**. A label whose source is a **Local quote** is not a **Chain quote**
- A **Grid** is the series of quotes used to sample a **Price curve**. A **Knot** is a grid point a **Sample formula** is drawn through. A **Holdout** is a grid point that is not a knot
- A **Quote record** stores **Chain quotes** for checking a price curve. Building a **Price curve** does not read a quote record
- An **Estimate** is read from the stored **Price curve**. A **Local quote** is read from the **Quote inputs**
- A **Canonical pool** is a shape whose **Structural formula** is determined by the quote inputs. A **Metric pool**, a **Crypto pool**, and a **Failed read** are not canonical pools
- A **Bloom hit** becomes pool identities only after the **Receipt bundle** is read and a **Creation event** matches
- The **Corpus** is a partial set of **Receipt bundles**. It does not replace a **List** or a bloom walk
- A **Hole** does not move a **Cursor**, and a hole is not a gap in the **Header chain**
- Before the **Merge**, receipt history for a bloom hit comes from an **Era1 file**. After the **Merge**, it comes from a **Peer**. The **Block body** is not stored on either path
- A **Published node** becomes a **Summary** only through **Sightings**. A served block outranks an advertised **History window**
- The **Database service** owns the **Header chain**, the **Corpus**, the **Discovery store**, the **Venue catalog**, and the **Work queue**
- The **Peer service** owns the **Peer directory** and is the only dialer
- A **Client** does not open those files. A **Colocated pipeline** calls the store in process
- **Discovery** writes **Pool identities**. The **State pipeline** then writes **Configuration**, **Quoting state**, the **Tick book**, the **Price curve**, and the **Pair conduit**. The **Watch** is another producer of **Dirty** marks
- A **Head class** with an unchanged **Tick bitmap** keeps the **Tick book**. A **Tick class**, or a changed bitmap, reads **Tick words** again. Tick class replaces head class
- A new **Generation** drops the **Price curve**. The **Quote record** stays until the **Pool identity** is deleted. Dropping the price curve queues the **Pair conduit** again
- A **Pair conduit** merges the direct actions for one ordered pair of tokens. A path through a third token is a composition of pair conduits
- A **Slope segment** is one step of that merge. A **Meta-action** is the stored edge. A pool direction is a member of a meta-action
- An **Action** spends tokens and pays tokens in one transaction. Amounts are not part of it. A **Pair conduit** stores the swaps between one ordered pair. A **Liquidity mint**, a **Vault deposit**, and an **Order fill** are actions a pair conduit does not store
- A **Basket** counts the outputs a set of holdings can still pay. The count is the minimum of the last counts
- **Depletion** is the reserve a quote uses. A **Trading set** quotes a direction from its original reserves. Same-direction tenders are one submission. The accrued-fee update is not that ledger
- A **Hub label** is the directed quote from a token to one hub. It is not a stage. The choice among the direct quote, one shared hub, and two hubs is the maximum of the cleared cross keys
- A **Stage** is configuration, quoting state, the tick book, **Fit**, or the pair conduit. There is no live stage for chain quotes
- A **Sample catalog** is a copy of identities for a trial. It is not the **Live catalog**

## Example dialogue

> **Dev:** "When we build a price curve, do we call the quoter at a few sizes?"
> **Domain expert:** "No. **Fit** stores a **Price curve** from the **Quote inputs**: the **Configuration**, the **Quoting state**, and the **Tick book**. A **Local quote** is an output computed from those records. **Selection** may score formulas on a **Grid** of local quotes. A **Chain quote** is a **Quote record**, kept for checking, and it is not an input to fit."

> **Dev:** "So the product formula is the reserves and the fee?"
> **Domain expert:** "The **Product formula** is the formula of a **Constant-product pool**. That formula is determined by the two **Reserves** and the **Pool fee**. The reserves are inputs. They are not the formula."

> **Dev:** "And a concentrated pool?"
> **Domain expert:** "The **Ladder** is determined by the **Price**, the in-range **Liquidity**, and the **Tick words**. The price and the liquidity are on the **Quoting state**. The tick words are the **Tick book**. The **Tick bitmap**, also on the quoting state, only names which ticks exist. A **Hook** is configuration. The **Protocol fee** is quoting state. Neither is a parameter of the ladder."

> **Dev:** "What is the grid?"
> **Domain expert:** "The **Grid** is the series of quotes used to sample the **Price curve**. Each point is a fraction of the **Sampling ceiling**. A **Holdout**, at one quarter or four fifths of that ceiling, is scored and is not used to choose parameters. A **Knot** is a grid point a **Sample formula** is drawn through."

> **Dev:** "What about a pool with nothing to price?"
> **Domain expert:** "An **Unpriced** quoting state keeps the **Pool identity** and stores no **Price curve**. A **Skipped curve** is a stored price curve that names no **Formula**. A **Failed read** means the detail read's fields did not come back."

## Example dialogue: finding a pool

> **Dev:** "How does a pool get an identity?"
> **Domain expert:** "If the **Factory** keeps a **List**, the **Index lens** reads it. If the factory keeps no list, a **Bloom hit** names a block and the **Creation event** in the **Receipt bundle** is the identity. A **Registry** covers Curve pools no factory event named."

> **Dev:** "Is a CoW auction a list of pools?"
> **Domain expert:** "No. That file is a **Settlement**. It is a hint about which venues trade. It is not a **Pool identity** and it is not a **List**."

> **Dev:** "The corpus already has Uniswap v3 logs. Is that the census?"
> **Domain expert:** "The **Corpus** is a partial set of **Receipt bundles**. **Discovery** can fold those creation events in. The census is the **Live catalog**."

> **Dev:** "People say hit for three different things."
> **Domain expert:** "A **Bloom hit** is a block that might contain a log. A **Discovery hit** is an **Asset** or a **Pool identity** read from a log that was there. The **Watch** marks a pool **Dirty** when a log is newer than its **Quoting state**."

## Example dialogue: history and a refresh

> **Dev:** "Do we sync chain state?"
> **Domain expert:** "No. **Chain state** stays on the node. We store a **Header chain**, **Receipt bundles** for some blocks, and the **Quote inputs** a pool quotes from."

> **Dev:** "Where do old receipts come from?"
> **Domain expert:** "Before the **Merge**, from an **Era1 file**. After the **Merge**, from a **Peer**. A **Block body** is not stored. A **Hole** means that block was not served, and the **Cursor** stays where it was."

> **Dev:** "A swap landed. What is dirty?"
> **Domain expert:** "The **Watch** reads logs the **Peer service** already has. A swap or a sync is a **Head class**. A mint or a burn is a **Tick class**. Tick class replaces head class."

> **Dev:** "Does that reload ticks and throw away quotes?"
> **Domain expert:** "A head class with the same **Tick bitmap** keeps the **Tick book**. A tick class reads tick words again. The new **Generation** drops the **Price curve**. The **Quote record** stays."

## Flagged ambiguities

- **"Curve"** is a **Curve pool** or, loosely, a **Price curve**. Say one of those. "Curve" alone is not a term.
- **"Quote"** is a **Local quote**, a **Chain quote**, a **Quote record**, or the verb. Name the object. A **Label** is what **Selection** scores, and the label names its source. An amount computed from the quote inputs is a **Local quote** even when a stored field is still called chain output.
- **"Catalog"** is the **Venue catalog**, the **Sample catalog**, or the **Peer directory**. Say which.
- **"Generation"** identifies one **Quoting state**. A version identifies the schema of a **Price curve** or a **Quote record**. A generation is not a version.
- **"Head"** is the newest block a **Peer** reports, or a **Head class**. The block-pinned record is the **Quoting state**.
- **"Hit"** is a **Bloom hit**, a **Discovery hit**, or a **Watch** mark.
- **"Sample"** is a **Sample catalog**, a **Chain quote**, or one point of a **Grid**.
- **"Body"** is a **Block body**, which is not stored, or the era1 receipt bytes in the **Discovery store**. The corpus object is a **Receipt bundle**.
- **"Peer"** is a node. The **Peer service** is the process. The **Peer directory** is the sightings and summaries.
- **"Index"** is a **List** or a position in a **Tick bitmap**. A list names pools. A bitmap names ticks.
- **"Canonical"** is a **Canonical factory** or a **Canonical pool**. The factory is an emitter we accept. The pool is a shape whose structural formula is determined by the quote inputs.
- **"State"** is **Chain state**, **Quoting state**, or the **State pipeline**. The quoting state is one record. The state pipeline is the work that writes configuration, quoting state, the tick book, the price curve, and the pair conduit.
- The joined inputs a local quote reads are the **Quote inputs**. **Configuration**, **Quoting state**, and **Tick book** name the individual records.
- **"Details"** is the detail read. That read writes **Configuration** and **Quoting state** together. The **Tick book** is the following read.
- **"Sync"** is a refresh of **Quoting state**, or the read of the **Tick book**. Say which record.
- **Discovery** is the pipeline that writes pool identities. A **Discovery hit** is one log's asset or pool identity.
- **"Fit"** is the stage that stores a **Price curve**. The choice among formulas inside that stage is **Selection**.
- **"The function"** is not a name for a **Price curve** or a **Formula**.
- **Quote stage.** Older work named a stage that fetched chain quotes. That stage is not claimed. Chain quotes already stored remain **Quote records**.
- **"Route"** and **"hop"** are not names for a **Pair conduit**. The conduit is one ordered pair of tokens. A path through a third token is a composition of pair conduits. A **Basket** is the count of fixed-weight outputs. It is not a pair conduit.
- **Unequal-weight Balancer.** The local quote and the stored weighted formula are both determined by balances, weights, and the pool fee, and they are different evaluations. Selection can store a **Sample formula** instead. Do not treat the local quote and the stored formula as one object.
- **Crypto pool.** Its local quote is a spot line on the price scales. That line is not the crypto invariant and not a chain quote.
- **Kind and shape.** Kind is the family on the pool identity. Shape is the quoting rule of the pool.
