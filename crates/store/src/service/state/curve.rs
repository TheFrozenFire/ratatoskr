//! Stored price curves.

use redb::{Durability, ReadableTable};
use store_model::{decode_curve_record, decode_mutable, encode_curve, entity_key, CurveRecord};

use super::super::catalog::Catalog;
use super::*;

impl Catalog {
    pub fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<Option<CurveRecord>> {
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
        Ok(previous)
    }

    /// Store a page of curves in one commit. A generation mismatch stores none of them.
    pub fn put_curves(&self, curves: &[CurveRecord]) -> eyre::Result<Vec<Option<CurveRecord>>> {
        if curves.is_empty() {
            return Ok(Vec::new());
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
        Ok(replaced)
    }
}
