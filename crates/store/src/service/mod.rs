//! Domain services. Each service speaks [`store_model`] records.
//!
//! [`Feeds`] stores directory rows, proxy configuration, phases, and answers.
//! [`Spots`] stores one priced round trip per token and numeraire.
//! [`HeaderChain`] stores the checked header chain. [`Corpus`] stores receipt
//! blocks. A caller parses, prices, or follows logs, then calls the service.

mod catalog;
mod conduit;
mod corpus;
mod feeds;
mod headers;
mod hooks;
mod peers;
mod pools;
mod query;
mod shelf;
mod spots;
mod state;
mod work_read;

pub use catalog::{
    append_dropped_ids, append_fill_ids, fill_identities, read_fill_ids, sample_identities,
    write_fill_ids, Catalog, DiscoveryRead, EntityRead, FillReport, SampleReport,
};
pub use conduit::{
    ConduitBody, ConduitMember, ConduitMeta, ConduitRow, CurvePage, CurveRow, MemberTarget,
    StoredConduit,
};
pub use corpus::Corpus;
pub use feeds::Error as FeedError;
pub use feeds::{FeedDetail, FeedLine, FeedRead, Feeds};
pub use headers::HeaderChain;
pub use hooks::{install_capacity, Capacity};
pub use peers::{PeerRead, PeerSummary, Peers};
pub use pools::Pools;
pub use query::{ConduitPoint, ConduitRead, ConduitScan, Snapshot, TokenRows, VenuePoint};
pub use shelf::Shelf;
pub use spots::Error as SpotError;
pub use spots::{SpotRead, Spots};
pub use state::PoolCard;
pub use work_read::WorkRead;
