# Problem

Ratatoskr starts from raw chain data and produces objects a searcher can quote without enumerating venues. The search is from one token to another. The expensive part of a naive search is combinatorial quoting. The objects below are the precomputation that removes that work, and the questions that precomputation does not yet answer.

A venue here is a pool in the catalog. A route is a sequence of trades from an input token to an output token. A price curve is the output a quote assigns across input sizes.

## Raw data

Ingestion stores the pool details a quote needs. From those details a simulator can price a trade through that venue. The price curve of one venue is determined by that data, so it can be computed before any search.

## Naive search

A naive search looks for a raw route between two tokens. It starts at one token and enumerates venues that trade the input for the output. It also works outward: token A to token C, then token C to token B, and further compositions of that kind. Each candidate is quoted. The search keeps the candidate with the better price impact.

That enumeration is combinatorial. The same input can also be split across several venues. At larger sizes that split often pays more than giving the whole input to the single best venue. A search that quotes each route as a whole, and each venue as a separate routing entity, repeats the split for every candidate.

## What the search actually needs

The search wants actions that take one token to another. A venue is one such action. Several venues between the same two tokens are interchangeable inputs to one allocation: the next unit of input goes to whichever action still pays the most for it.

Two precomputations follow.

The first is a price curve for each venue. Quoting that venue during a search reads the curve.

The second collects every direct action between one ordered pair of tokens and allocates an input across them in advance. That object is a pair conduit.

## Pair conduit

A pair conduit is named by two identity tokens, in order. In this model its members are the direct venues between those tokens. A multi-hop route is not a member. Whether any multi-hop route belongs inside a pair conduit is open.

The allocation orders the marginal pieces of the member curves. The next unit of input goes to the remaining piece with the highest slope, whichever venue that piece belongs to. A venue receives more input later when one of its later pieces is still the steepest piece remaining. For each input size the result is the output of that allocation.

That output is the best these direct venues can produce together when each action's marginal falls as its own input grows, and the actions do not share a pool. When a later piece of a venue curve is steeper than an earlier piece, the curve that enters the allocation is the left-greedy minorant: the prefix whose slopes already fall, with each later piece lowered until its slope no longer rises. Once a slope rises, no increasing concave curve under the samples dominates the others.

That allocation is itself a price curve, the whole-conduit curve. A later quote reads the conduit curve. It does not quote the member venues again. The conduit is the routing entity the search sees for that ordered pair.

A conduit named by more than two identity tokens is not part of this model. Whether one is useful is open.

## Hub

A hub is a token that is highly connected. The definition is open. It may combine two facts: the token trades against many other tokens, and those connections are deep. Native ETH and the popular stablecoins are the obvious examples. Hubs are also connected to each other deeply.

The routing value of a hub is that definition. Its edges offer many ways onward, and those ways have deep liquidity. A searcher that leaves the direct pair will look through such tokens. The quote from a token to a hub, and from that hub onward, is work the search would do anyway. Precomputing it stores the price curve of that quote so the search reads the curve instead of rebuilding it.

How a token qualifies as a hub is not fixed. Connectivity alone, depth alone, and any mix of the two are still candidates.

## What a searcher still composes

After these objects exist, a search from token A to token B can read the pair conduit for that ordered pair. It can also read the precomputed quotes through hubs: A to a hub, that hub to B, and one hub to another hub when the path uses two of them.

A route that is neither the direct pair nor a path through a hub is outside this model. Whether any such route is worth keeping is open. Other fundamental route types may exist, and this model does not name them.
