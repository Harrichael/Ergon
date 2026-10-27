use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail};
use rusqlite::Connection;

use crate::project::{self, Project};
use crate::slug;

/// A task reference: `slug` or `project/slug`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRef {
    pub project: Option<String>,
    pub slug: String,
}

impl FromStr for TaskRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (project, slug) = match s.split_once('/') {
            Some((p, slug)) => {
                project::validate_name(p)?;
                (Some(p.to_string()), slug)
            }
            None => (None, s),
        };
        if !slug::is_valid(slug) {
            bail!("invalid task slug {slug:?}");
        }
        Ok(TaskRef {
            project,
            slug: slug.to_string(),
        })
    }
}

/// References separated by commas: `FTFE,nHcx` or `demo/FTFE,demo/nHcx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRefs(pub Vec<TaskRef>);

impl FromStr for TaskRefs {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        s.split(',')
            .map(str::parse)
            .collect::<Result<_>>()
            .map(TaskRefs)
    }
}

impl TaskRefs {
    pub fn iter(&self) -> impl Iterator<Item = &TaskRef> {
        self.0.iter()
    }

    pub fn slugs(&self) -> Vec<String> {
        self.0.iter().map(|r| r.slug.clone()).collect()
    }
}

/// The single project that `--project` and the references name, falling back
/// to the nearest `.ergon` file above `cwd`.
pub fn project_for(
    conn: &Connection,
    flag: Option<&str>,
    cwd: &Path,
    refs: &[&TaskRef],
) -> Result<Project> {
    let mut names: Vec<&str> = refs.iter().filter_map(|r| r.project.as_deref()).collect();
    names.extend(flag);
    names.sort();
    names.dedup();
    match names.as_slice() {
        [] => project::current(conn, None, cwd),
        [name] => project::by_name(conn, name),
        _ => bail!(
            "tasks must be in one project, but these name several: {}",
            names.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_refs() {
        let r: TaskRef = "E6dn".parse().unwrap();
        assert_eq!(
            r,
            TaskRef {
                project: None,
                slug: "E6dn".into()
            }
        );
        let r: TaskRef = "ergon/root".parse().unwrap();
        assert_eq!(r.project.as_deref(), Some("ergon"));
        for bad in ["E6d", "E0dn", "a b/E6dn", "/E6dn", "ergon/"] {
            assert!(bad.parse::<TaskRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn refs_must_agree_on_project() {
        let mut conn = crate::db::open_in_memory().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        project::init(&mut conn, "a", "x", a.path()).unwrap();
        project::init(&mut conn, "b", "x", b.path()).unwrap();
        let in_a: TaskRef = "a/E6dn".parse().unwrap();
        let in_b: TaskRef = "b/E6dn".parse().unwrap();
        let bare: TaskRef = "E6dn".parse().unwrap();

        assert_eq!(
            project_for(&conn, None, b.path(), &[&in_a]).unwrap().name,
            "a"
        );
        assert_eq!(
            project_for(&conn, None, b.path(), &[&bare]).unwrap().name,
            "b"
        );
        assert_eq!(
            project_for(&conn, Some("a"), b.path(), &[&bare, &in_a])
                .unwrap()
                .name,
            "a"
        );
        assert!(project_for(&conn, None, a.path(), &[&in_a, &in_b]).is_err());
        assert!(project_for(&conn, Some("b"), a.path(), &[&in_a]).is_err());
    }
}
