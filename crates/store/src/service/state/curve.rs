//! Curves and chain-sample quotes.

use redb::{Durability, ReadableTable};
use store_model::{
    decode_curve_record, decode_mutable, encode_curve, encode_quote, entity_key, CurveRecord,
    QuoteRecord,
};

use super::super::catalog::Catalog;
use super::*;

impl Catalog {
    pub fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let previous = {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut curves = write.open_table(CURVES)?;
            let key = entity_key(curve.kind, &curve.id);
            let Some(stored) = mutable_table.get(key.as_slice())? else {
                return Err(eyre::eyre!("curve has no mutable row"));
            };
            let view = decode_mutable(stored.value())?;
            if view.generation != curve.generation {
                return Err(eyre::eyre!("curve generation"));
            }
            let previous = match curves.get(key.as_slice())? {
                Some(value) => decode_curve_record(curve.kind, curve.id, value.value()).ok(),
                None => None,
            };
            let bytes = encode_curve(curve);
            curves.insert(key.as_slice(), bytes.as_slice())?;
            previous
        };
        write.commit()?;
        super::super::conduit::retarget(
            self,
            curve.kind,
            curve.id,
            previous.as_ref(),
            Some(curve),
        )?;
        Ok(())
    }

    /// Store a page of curves in one commit. A generation mismatch stores none of them.
    pub fn put_curves(&self, curves: &[CurveRecord]) -> eyre::Result<()> {
        if curves.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let mut replaced = Vec::with_capacity(curves.len());
        {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut table = write.open_table(CURVES)?;
            for curve in curves {
                let key = entity_key(curve.kind, &curve.id);
                let Some(stored) = mutable_table.get(key.as_slice())? else {
                    return Err(eyre::eyre!("curve has no mutable row"));
                };
                let view = decode_mutable(stored.value())?;
                if view.generation != curve.generation {
                    return Err(eyre::eyre!("curve generation"));
                }
                let previous = match table.get(key.as_slice())? {
                    Some(value) => decode_curve_record(curve.kind, curve.id, value.value()).ok(),
                    None => None,
                };
                let bytes = encode_curve(curve);
                table.insert(key.as_slice(), bytes.as_slice())?;
                replaced.push(previous);
            }
        }
        write.commit()?;
        for (previous, curve) in replaced.iter().zip(curves) {
            super::super::conduit::retarget(
                self,
                curve.kind,
                curve.id,
                previous.as_ref(),
                Some(curve),
            )?;
        }
        Ok(())
    }

    pub fn put_quote(&self, quote: &QuoteRecord) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut quotes = write.open_table(QUOTES)?;
            let key = entity_key(quote.kind, &quote.id);
            let Some(stored) = mutable_table.get(key.as_slice())? else {
                return Err(eyre::eyre!("quote has no mutable row"));
            };
            let view = decode_mutable(stored.value())?;
            if view.generation != quote.generation || view.block != quote.block {
                return Err(eyre::eyre!("quote generation"));
            }
            let bytes = encode_quote(quote);
            quotes.insert(key.as_slice(), bytes.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Store a page of quotes in one commit. A generation mismatch stores none of them.
    pub fn put_quotes(&self, quotes: &[QuoteRecord]) -> eyre::Result<()> {
        if quotes.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mutable_table = write.open_table(MUTABLE)?;
            for quote in quotes {
                let key = entity_key(quote.kind, &quote.id);
                let Some(stored) = mutable_table.get(key.as_slice())? else {
                    return Err(eyre::eyre!("quote has no mutable row"));
                };
                let view = decode_mutable(stored.value())?;
                if view.generation != quote.generation || view.block != quote.block {
                    return Err(eyre::eyre!("quote generation"));
                }
            }
            let mut quote_table = write.open_table(QUOTES)?;
            for quote in quotes {
                let key = entity_key(quote.kind, &quote.id);
                let bytes = encode_quote(quote);
                quote_table.insert(key.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Remove one quote row. The snapshot and the curve stay.
    pub fn delete_quote(&self, kind: u8, id: B256) -> eyre::Result<bool> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let removed = {
            let mut quotes = write.open_table(QUOTES)?;
            let key = entity_key(kind, &id);
            let removed = quotes.remove(key.as_slice())?.is_some();
            removed
        };
        write.commit()?;
        Ok(removed)
    }
}
