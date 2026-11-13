//! ergond: keeps ergon's copy of choros's landings up to date as they
//! change, and sets tasks done as their landings merge (see `ergon::sync`).

fn main() {
    let run = ergon::db::default_path().and_then(|db| ergon::sync::daemon(&db));
    if let Err(e) = run {
        eprintln!("ergond: {e:#}");
        std::process::exit(1);
    }
}
