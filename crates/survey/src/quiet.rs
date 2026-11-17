//! Progress lines for the survey command. The status screen silences them.

use std::cell::Cell;

thread_local! {
    static SILENT: Cell<bool> = const { Cell::new(false) };
}

pub fn silence() {
    SILENT.set(true);
}

pub fn say(message: std::fmt::Arguments<'_>) {
    if SILENT.with(Cell::get) {
        return;
    }
    eprintln!("{message}");
}
