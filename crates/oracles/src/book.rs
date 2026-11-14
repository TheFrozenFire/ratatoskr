//! What the directory pass and the answer follow ask of the feed file.

use crate::records::{Answer, Error, Feed, Gauges, Listing, ObservedAnswer, Phase, Read};
use alloy_primitives::Address;

pub trait FeedBook {
    fn apply_directory(&self, body: &str) -> Result<Listing, Error>;
    fn remember_answers(&self) -> Result<u64, Error>;
    fn gauges(&self) -> Result<Gauges, Error>;
    fn proxies(&self) -> Result<Vec<Address>, Error>;
    fn apply_read(&self, proxy: Address, read: Read) -> Result<(), Error>;
    fn follow_block(&self) -> Result<Option<u64>, Error>;
    fn lookback_done(&self) -> Result<bool, Error>;
    fn mark_lookback(&self) -> Result<(), Error>;
    fn commit_follow(&self, next_block: u64, observed: &[ObservedAnswer]) -> Result<usize, Error>;
    fn answers(&self, proxy: Address) -> Result<Vec<Answer>, Error>;
    fn phases(&self, proxy: Address) -> Result<Vec<Phase>, Error>;
    fn feed(&self, proxy: Address) -> Result<Option<Feed>, Error>;
}
