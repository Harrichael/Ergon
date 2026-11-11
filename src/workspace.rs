//! The choros workspace `ergon work` makes for tasks it starts, bound to
//! them: every `choros land` from it runs `ergon land-hook` for those
//! tasks (see [`crate::land`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::task::Tasks;

/// The land hook of a bound workspace, before the tasks' references.
pub const HOOK: &str = "ergon land-hook --task ";

/// Which repos the workspace is made with.
pub enum Repos {
    Given(Vec<String>),
    /// Picked in choros's own picker.
    Pick,
    /// The project's default, if it has one.
    Default(Vec<String>),
}

/// The part of `choros here --json` ergon reads.
#[derive(Deserialize)]
struct Here {
    kind: String,
}

/// Makes a workspace in `cwd` for `ids` and binds it to them, returning
/// the folder to start their session in. `None` when `cwd` isn't a place
/// choros makes workspaces in: choros isn't installed, this is no trunk of
/// its, or it is already inside a workspace, where the session starts as
/// it is.
pub fn make(
    tasks: &Tasks,
    ids: &[i64],
    cwd: &Path,
    name: Option<&str>,
    repos: Repos,
) -> Result<Option<PathBuf>> {
    if crate::on_path(Path::new("choros")).is_none() {
        return Ok(None);
    }
    let here = Command::new("choros")
        .args(["here", "--json"])
        .current_dir(cwd)
        .output()
        .context("running choros here")?;
    let Some(here) = here
        .status
        .success()
        .then(|| serde_json::from_slice::<Here>(&here.stdout).ok())
        .flatten()
    else {
        return Ok(None);
    };
    if !matches!(here.kind.as_str(), "trunk" | "branch-space") {
        return Ok(None);
    }

    let name = match name {
        Some(n) => n.to_string(),
        None => name_for(tasks, ids, cwd),
    };
    let mut work = Command::new("choros");
    work.arg("work")
        .current_dir(cwd)
        // Its picker reads keys.
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    match repos {
        Repos::Pick => {
            work.args(["-i", &name]);
        }
        Repos::Given(repos) | Repos::Default(repos) if !repos.is_empty() => {
            work.arg(&name).args(repos);
        }
        _ => bail!(
            "no repos to make the workspace {name} with: pass --repos REPO,…, pick them with \
             -i, or set the project's default with `ergon project repos REPO,…`. \
             --here starts the session in this folder instead"
        ),
    }
    let made = work.output().context("running choros work")?;
    let dir = String::from_utf8_lossy(&made.stdout).trim().to_string();
    if !made.status.success() {
        bail!("choros couldn't make the workspace {name}: what it said is above");
    }
    if dir.is_empty() {
        bail!("cancelled: no workspace was made");
    }
    let dir = PathBuf::from(dir);

    let refs: Vec<String> = ids.iter().map(|&id| tasks.reference(id)).collect();
    let hook = format!("{HOOK}{}", refs.join(","));
    let bound = Command::new("choros")
        .args(["hook", "land", &hook])
        .current_dir(&dir)
        .status()
        .context("running choros hook")?;
    if !bound.success() {
        bail!(
            "made the workspace {}, but couldn't bind it: `choros hook land \"{hook}\"` failed there",
            dir.display()
        );
    }
    Ok(Some(dir))
}

/// A workspace name from the first task's title: its first few words, and
/// the task's slug too when that folder is taken.
fn name_for(tasks: &Tasks, ids: &[i64], cwd: &Path) -> String {
    let task = tasks.get(ids[0]);
    let words: Vec<String> = task
        .title()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(|w| w.chars().take(16).collect::<String>().to_ascii_lowercase())
        .collect();
    let name = if words.is_empty() {
        task.slug.to_ascii_lowercase()
    } else {
        words.join("_")
    };
    if cwd.join(&name).exists() {
        format!("{name}_{}", task.slug)
    } else {
        name
    }
}
