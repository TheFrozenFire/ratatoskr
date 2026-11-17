//! One prepared catalog stage, measured one scenario at a time.
//!
//! The stage stays on this session so a later refresh does not scan again.
//! A whole-survey refresh calls [`Session::reload`] and then measures.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use eyre::WrapErr;
use searcher::Token;
use simulate::Price;
use store::service::{EntityRead, Snapshot};

use crate::measure;
use crate::scenario::Class;
use crate::stage::{self, Stage};

/// Name and class of one prepared scenario. Scores stay empty until it is measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub index: usize,
    pub name: String,
    pub class: &'static str,
}

/// Read-only survey over one entity snapshot.
pub struct Session {
    entities: EntityRead,
    snapshot: Snapshot,
    stage: Stage,
    spots: config::Spots,
    prices: BTreeMap<Token, Price>,
    missed: BTreeSet<Token>,
}

impl Session {
    /// Load the stage. Hub-reach census is skipped, the same as [`crate::measure`].
    pub fn open(
        data_dir: &Path,
        spots: &config::Spots,
        caches: &config::Store,
    ) -> eyre::Result<Self> {
        let entities = EntityRead::open_with(data_dir, caches.read).wrap_err("open catalog")?;
        let snapshot = entities.snapshot().wrap_err("entity snapshot")?;
        let stage = stage::load(&snapshot, spots, false)?;
        Ok(Self {
            entities,
            snapshot,
            stage,
            spots: spots.clone(),
            prices: BTreeMap::new(),
            missed: BTreeSet::new(),
        })
    }

    pub fn items(&self) -> Vec<Item> {
        self.stage
            .scenarios
            .iter()
            .enumerate()
            .map(|(index, scenario)| Item {
                index,
                name: scenario.name.clone(),
                class: class_name(scenario.class),
            })
            .collect()
    }

    /// Measure one scenario by its index in [`Session::items`].
    pub fn measure(&mut self, index: usize) -> eyre::Result<String> {
        measure::text_of(
            &self.snapshot,
            &mut self.stage,
            index,
            &mut self.prices,
            &mut self.missed,
        )
    }

    /// Read the catalog again and drop scores from the previous stage.
    pub fn reload(&mut self) -> eyre::Result<()> {
        let snapshot = self.entities.snapshot().wrap_err("entity snapshot")?;
        let stage = stage::load(&snapshot, &self.spots, false)?;
        self.snapshot = snapshot;
        self.stage = stage;
        self.prices.clear();
        self.missed.clear();
        Ok(())
    }
}

fn class_name(class: Class) -> &'static str {
    match class {
        Class::Ordinary => "ordinary",
        Class::Long => "long",
        Class::Via => "via",
        Class::Deep => "deep",
        Class::Isolated => "isolated",
    }
}
