# Problem

The core ingestion pipeline writes a pool identity, then a price curve for that pool, and follows the chain.

## Identity

Discovery writes a pool identity before the state pipeline can read that pool. An identity is a factory pool from a list or a creation event, a singleton pool, or a registry pool, for the venues the seed file names. A later local quote sees only identities discovery has stored.

## Settlement

A settlement records fills. It is not a pool the state pipeline fits.

## Quote inputs

The quote inputs are the configuration, the quoting state, and the tick book at one generation. Fit stores the price curve from local quotes of those inputs. The curve is in the venue catalog before any search reads it.

## Chain quote

A chain quote checks a price curve. Building the price curve does not read the quote record. There is no live stage that fetches chain quotes. [docs/quotes.md](docs/quotes.md) says what a chain quote is.

## Native ETH

Native ETH is the zero address. Wrapped ETH is a different token. The one-for-one custodial wrap is an action, not a pool the fit stage stores.

## The chain

The pipeline follows the chain. After a pass, discovery reads lists, blooms, and token details again as the header cursor advances. The watch marks a pool dirty when a receipt log is newer than its quoting state. A new generation drops the price curve. `--until-idle` is an explicit stop after the queue drains. It is not the steady process. A state pipeline with no peer service and no header sync does not see new blocks.

The search problem is [docs/search.md](docs/search.md). The flow is [ARCHITECTURE.md](ARCHITECTURE.md).
