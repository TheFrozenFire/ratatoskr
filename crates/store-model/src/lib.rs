//! Records the store services read and write.
//!
//! A record is the value a file holds. Parsing a directory, following a log,
//! and pricing a round trip stay in the crates that do that work.

pub mod chain;
pub mod conduit;
pub mod feed;
pub mod peer;
pub mod pool;
pub mod spot;
pub mod state;
pub mod tick;
pub mod venue;

pub use feed::{
    Answer, Configuration, DirectoryRow, Feed, Gauges, HistoryJob, Listing, Mismatch,
    ObservedAnswer, Phase, ProxyHead, Read,
};
pub use spot::Spot;

pub use chain::{
    decode_block_record, decode_block_records, decode_header_record, decode_header_records,
    encode_block_record, encode_header_record, encode_header_records, BlockRecord, HeaderRecord,
    MAINNET_GENESIS_HASH,
};

pub use peer::{
    PublishedPeer, Sighting, Summary, DIAL_FAILED, DROPPED, EMPTY, SAW_STATUS, SERVED, TOO_MANY,
};
pub use pool::PoolRecord;

pub use conduit::{decode_conduit_job, encode_conduit_job, ConduitFail, ConduitJob};
pub use state::{
    decode_curve_record, decode_dirty, decode_job, decode_mutable, decode_mutable_parts,
    decode_quote_record, decode_tick_words, encode_curve, encode_dirty, encode_job, encode_mutable,
    encode_quote, encode_tick_page, priority_of, tick_page_generation, CurveRecord, Dirty,
    ImmutablePut, ImmutableRow, MutableStored, MutableView, MutableWrite, PoolState, QuoteRecord,
    StateSummary, TickPage, WatchHit, WorkFail, WorkJob, CLASS_HEAD, CLASS_NONE, CLASS_TICKS,
    CURVE_VERSION, DIRTY_BAND, QUOTE_VERSION, STAGE_CONDUIT, STAGE_CURVE, STAGE_IMMUTABLE,
    STAGE_MUTABLE, STAGE_QUOTE, STAGE_TICKS, TICK_PAGE,
};
pub use tick::TickWord;
pub use venue::{
    decode_body, decode_row, decode_token, encode_body, encode_row, encode_token, entity_key,
    factory_of, kind_name, tally_kind, Put, ReceiptBody, TokenRecord, VenueRow, AERO, BALANCER_V2,
    BALANCER_V3, CURVE, FLAG_OBS, METRIC, PANCAKE_V2, PANCAKE_V3, RECORD_VERSION,
    RECORD_VERSION_OBS, SUSHI_V2, SUSHI_V3, UNI_V2, UNI_V3, UNI_V4, V2_FORK, V3_FORK,
};
