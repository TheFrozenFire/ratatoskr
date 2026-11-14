//! Spots live in `spots.redb`. The feed answer table is not in that file.

use alloy_primitives::Address;
use num_bigint::BigInt;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use spots::{Spot, Spots};

fn token(byte: u8) -> Address {
    let mut raw = [0_u8; 20];
    raw[19] = byte;
    Address::from(raw)
}

fn spot(token_byte: u8, numeraire_byte: u8, bought: u32) -> Spot {
    Spot {
        token: token(token_byte),
        numeraire: token(numeraire_byte),
        size: BigInt::from(10).pow(20),
        bought: BigInt::from(bought),
        sold: BigInt::from(7),
        hops: 2,
        block: 42,
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ratatoskr-spots-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_pass_replaces_the_spots_and_leaves_the_answer_table_absent() {
    let dir = temp_dir("store");
    let path = dir.join("spots.redb");
    let kept = spot(1, 2, 9);
    let gone = spot(3, 2, 4);
    let book = Spots::open(&path).unwrap();
    book.replace(&[gone.clone(), kept.clone()]).unwrap();
    let mut stored = book.read().unwrap();
    stored.sort_by_key(|spot| spot.token);
    assert_eq!(stored, vec![kept.clone(), gone]);
    book.replace(&[kept.clone()]).unwrap();
    assert_eq!(book.read().unwrap(), vec![kept.clone()]);
    drop(book);

    let db = redb::Database::open(&path).unwrap();
    let read = db.begin_read().unwrap();
    let answers: TableDefinition<&[u8], &[u8]> = TableDefinition::new("answers");
    match read.open_table(answers) {
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Ok(_) => panic!("spots file has an answers table"),
        Err(err) => panic!("{err}"),
    }
    let spots: TableDefinition<&[u8], &[u8]> = TableDefinition::new("spots");
    let table = read.open_table(spots).unwrap();
    assert_eq!(table.iter().unwrap().count(), 1);
    drop(table);
    drop(read);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
