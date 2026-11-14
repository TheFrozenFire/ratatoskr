//! Domain services. Each service owns one file and speaks [`store_model`] records.
//!
//! [`Feeds`] stores directory rows, proxy configuration, phases, and answers.
//! [`Spots`] stores one priced round trip per token and numeraire. A caller
//! parses, prices, or follows logs, then calls the service. The feed service
//! does not fetch a directory. The spot service does not quote a conduit.

mod feeds;
mod spots;

pub use feeds::Error as FeedError;
pub use feeds::Feeds;
pub use spots::Error as SpotError;
pub use spots::Spots;
