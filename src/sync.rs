//! Keeping ergon's copy of choros's landings (see [`crate::landings`]) up
//! to date, from each trunk's event log (`choros events`): by `ergond` as
//! the events come, or by `ergon sync` when asked. A landing task whose
//! landings have all merged is then done.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::landings;
use crate::ops::{self, Changes};
use crate::project::{self, Project};
use crate::task::{Status, Tasks};

pub fn choros_installed() -> bool {
    crate::on_path(Path::new("choros")).is_some()
}

/// Every choros trunk on this machine.
pub fn trunks() -> Result<Vec<PathBuf>> {
    let out = Command::new("choros")
        .arg("trunks")
        .output()
        .context("running choros trunks")?;
    if !out.status.success() {
        bail!(
            "choros trunks failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Takes in what `trunk`'s log has gained since it was last read, and
/// returns how many events that was and whether any told of a merge.
pub fn catch_up(conn: &mut Connection, trunk: &Path) -> Result<(usize, bool)> {
    let since = landings::read_through(conn, trunk)?;
    let (lines, again) = match events(trunk, since) {
        Ok(lines) => (lines, false),
        // The log isn't the one that was read before: a trunk made again
        // at the same path. Its events replace what was kept.
        Err(e) if since > 0 && format!("{e:#}").contains("holds no event") => {
            (events(trunk, 0)?, true)
        }
        Err(e) => return Err(e),
    };
    let tx = conn.transaction()?;
    if again {
        landings::forget(&tx, trunk)?;
    }
    let mut merged = false;
    for line in &lines {
        merged |= landings::apply(&tx, trunk, line)?.1;
    }
    tx.commit()?;
    Ok((lines.len(), merged))
}

fn events(trunk: &Path, since: i64) -> Result<Vec<String>> {
    let out = Command::new("choros")
        .args(["events", "--since", &since.to_string()])
        .current_dir(trunk)
        .output()
        .with_context(|| format!("running choros events in {}", trunk.display()))?;
    if !out.status.success() {
        bail!(
            "choros events failed in {}: {}",
            trunk.display(),
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .trim_start_matches("choros: ")
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Sets `p`'s landing tasks whose landings have all merged to done, and
/// returns a line for each: what was done, or why it couldn't be.
pub fn finish_merged(conn: &mut Connection, p: &Project) -> Result<Vec<String>> {
    let Some(all) = landings::all(conn)? else {
        return Ok(Vec::new());
    };
    let tasks = Tasks::load(conn, p)?;
    let mut said = Vec::new();
    for id in landings::merged(&tasks, &all) {
        let r = tasks.reference(id);
        let done = Changes {
            status: Some(Some(Status::Done)),
            ..Default::default()
        };
        said.push(match ops::edit(conn, p, &tasks.get(id).slug, done) {
            Ok(_) => format!("{r} done: its landings merged"),
            Err(e) => format!("couldn't mark {r} done: {e:#}"),
        });
    }
    Ok(said)
}

fn finish_all(conn: &mut Connection) -> Result<Vec<String>> {
    let mut said = Vec::new();
    for p in project::list(conn, false)? {
        said.extend(finish_merged(conn, &p)?);
    }
    Ok(said)
}

/// `ergon sync`: reads every trunk's log to its end. Returns what to say.
pub fn run(conn: &mut Connection) -> Result<Vec<String>> {
    if !choros_installed() {
        bail!("choros isn't on PATH, so there are no landings to read");
    }
    let mut said = Vec::new();
    for trunk in trunks()? {
        let (n, _) = catch_up(conn, &trunk)?;
        if n > 0 {
            said.push(format!(
                "{}: {n} {} from choros",
                trunk.display(),
                if n == 1 { "event" } else { "events" }
            ));
        }
    }
    said.extend(finish_all(conn)?);
    Ok(said)
}

/// Held by `ergond` for as long as it runs.
fn lock_path(db: &Path) -> PathBuf {
    db.with_file_name("ergond.lock")
}

/// The daemon's lock on the database at `db`, or `None` while another
/// holds it.
pub fn lock(db: &Path) -> Result<Option<File>> {
    let path = lock_path(db);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(file.try_lock().is_ok().then_some(file))
}

/// What to warn of before showing landings nothing is keeping up to date.
pub fn stale(db: &Path) -> Option<&'static str> {
    (choros_installed() && matches!(lock(db), Ok(Some(_))))
        .then_some("ergond isn't running, so landings may be out of date; `ergon sync` catches up")
}

/// `ergond`: follows every trunk's log for as long as it runs.
pub fn daemon(db: &Path) -> Result<()> {
    let Some(_lock) = lock(db)? else {
        bail!("ergond is already running on {}", db.display());
    };
    // A trunk can be made, and choros installed, while this runs.
    let mut following: HashMap<PathBuf, std::thread::JoinHandle<()>> = HashMap::new();
    loop {
        if choros_installed() {
            match trunks() {
                Ok(trunks) => {
                    for trunk in trunks {
                        if following.get(&trunk).is_none_or(|t| t.is_finished()) {
                            let (db, at) = (db.to_path_buf(), trunk.clone());
                            log(&format!("following {}", trunk.display()));
                            following.insert(trunk, std::thread::spawn(move || follow(&db, &at)));
                        }
                    }
                }
                Err(e) => log(&format!("{e:#}")),
            }
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

/// Follows `trunk`'s log until it can't: the trunk is gone, say.
fn follow(db: &Path, trunk: &Path) {
    if let Err(e) = follow_log(db, trunk) {
        log(&format!("{}: {e:#}", trunk.display()));
    }
}

fn follow_log(db: &Path, trunk: &Path) -> Result<()> {
    let mut conn = crate::db::open(db)?;
    // What came while nothing followed, and a log that was begun again.
    let (n, _) = catch_up(&mut conn, trunk)?;
    if n > 0 {
        log(&format!("{}: {n} events", trunk.display()));
    }
    finish(&mut conn);
    let since = landings::read_through(&conn, trunk)?;
    let mut child = Command::new("choros")
        .args(["events", "--follow", "--since", &since.to_string()])
        .current_dir(trunk)
        .stdout(Stdio::piped())
        .spawn()
        .context("running choros events --follow")?;
    let lines = BufReader::new(child.stdout.take().expect("piped"));
    for line in lines.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match landings::apply(&conn, trunk, &line) {
            Ok((_, true)) => finish(&mut conn),
            Ok(_) => {}
            Err(e) => log(&format!("{}: {e:#}", trunk.display())),
        }
    }
    let status = child.wait()?;
    bail!("choros events --follow ended ({status}); following again shortly")
}

fn finish(conn: &mut Connection) {
    match finish_all(conn) {
        Ok(said) => said.iter().for_each(|line| log(line)),
        Err(e) => log(&format!("{e:#}")),
    }
}

fn log(msg: &str) {
    eprintln!("{} {msg}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
}
