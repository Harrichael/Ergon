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
