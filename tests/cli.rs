use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// An isolated database plus a working directory to run `ergon` in.
struct Env {
    home: TempDir,
}

impl Env {
    fn new() -> Self {
        Env {
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn dir(&self, rel: &str) -> std::path::PathBuf {
        let d = self.home.path().join(rel);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ergon"))
            .args(args)
            .current_dir(cwd)
            .env("ERGON_DB", self.home.path().join("ergon.db"))
            .output()
            .unwrap()
    }

    /// Runs `ergon` and returns stdout, panicking if it failed.
    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.run_in(cwd, args);
        assert!(
            out.status.success(),
            "ergon {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs `ergon` and returns stderr, panicking if it succeeded.
    fn err(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.run_in(cwd, args);
        assert!(
            !out.status.success(),
            "ergon {args:?} unexpectedly succeeded"
        );
        String::from_utf8(out.stderr).unwrap()
    }
}

#[test]
fn project_lifecycle() {
    let env = Env::new();
    let a = env.dir("a");
    let b = env.dir("b");

    env.ok(&a, &["init", "alpha", "Alpha intent"]);
    env.ok(&b, &["init", "beta", "Beta intent"]);
    assert!(a.join(".ergon").is_file());
    assert!(
        env.err(&a, &["init", "again", "x"])
            .contains("already exists")
    );

    let ls = env.ok(&a, &["project", "ls"]);
    assert!(ls.starts_with("alpha  ") && ls.contains("\nbeta  "), "{ls}");

    env.ok(&a, &["project", "rename", "alpha", "gamma"]);
    env.ok(&a, &["project", "retire", "gamma"]);
    let ls = env.ok(&a, &["project", "ls"]);
    assert!(!ls.contains("gamma"), "{ls}");
    let ls = env.ok(&a, &["project", "ls", "--all"]);
    assert!(ls.contains("gamma") && ls.contains("retired"), "{ls}");

    env.ok(&a, &["project", "unretire", "gamma"]);
    assert!(
        env.err(&a, &["project", "unretire", "gamma"])
            .contains("isn't retired")
    );
    assert!(
        env.err(&a, &["project", "rename", "gamma", "has space"])
            .contains("invalid project name")
    );
}

#[test]
fn task_commands() {
    let env = Env::new();
    let dir = env.dir("proj");
    let sub = env.dir("proj/deep/er");
    env.ok(&dir, &["init", "demo", "Root intent"]);

    let a = env.ok(&sub, &["add", "Backend\n\nDetails", "--priority", "p1"]);
    let a = a.trim();
    let b = env.ok(&dir, &["add", "API", "--parent", a]);
    let b = b.trim();
    let tree = env.ok(&dir, &["tree"]);
    assert!(
        tree.contains(&format!("  {a}  todo         p1  Backend")),
        "{tree}"
    );
    assert!(
        tree.contains(&format!("    {b}  todo         p-  API")),
        "{tree}"
    );

    let show = env.ok(&dir, &["show", a]);
    assert!(
        show.contains("Details") && show.contains(&format!("- {b}")),
        "{show}"
    );

    // References with an explicit project work from anywhere.
    let elsewhere = env.dir("elsewhere");
    env.ok(
        &elsewhere,
        &["edit", &format!("demo/{b}"), "--status", "landing"],
    );
    env.ok(
        &elsewhere,
        &["--project", "demo", "edit", a, "--priority", "none"],
    );
    assert!(
        env.err(&elsewhere, &["show", a])
            .contains("not in an ergon project")
    );

    let err = env.err(&dir, &["edit", a, "--status", "done"]);
    assert!(
        err.contains("refused") && err.contains("can't be landing"),
        "{err}"
    );
    let out = env.ok(&dir, &["edit", a, "--status", "cancelled"]);
    assert!(out.contains(&format!("Also cancelled demo/{b}")), "{out}");

    env.ok(&dir, &["mv", b, "root"]);
    assert!(env.err(&dir, &["mv", "root", b]).contains("can't be moved"));
    assert!(
        env.err(&dir, &["edit", "root", "--status", "none"])
            .contains("must have a status")
    );
    assert!(
        env.err(&dir, &["edit", "root", "--status", "nope"])
            .contains("invalid status")
    );
}

#[test]
fn dependency_commands() {
    let env = Env::new();
    let dir = env.dir("proj");
    env.ok(&dir, &["init", "demo", "Root"]);
    let a = env.ok(&dir, &["add", "A"]).trim().to_string();
    let b = env.ok(&dir, &["add", "B"]).trim().to_string();

    env.ok(&dir, &["dep", "add", &a, &b]);
    let show = env.ok(&dir, &["show", &a]);
    assert!(show.contains(&format!("Depends on:\n  - {b}")), "{show}");
    let show = env.ok(&dir, &["show", &b]);
    assert!(show.contains(&format!("Dependents:\n  - {a}")), "{show}");

    let err = env.err(&dir, &["dep", "add", &b, &a]);
    assert!(err.contains("wait on each other"), "{err}");
    env.ok(&dir, &["dep", "rm", &a, &b]);
    assert!(env.err(&dir, &["dep", "rm", &a, &b]).contains("doesn't depend"));
}
