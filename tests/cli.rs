use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// An isolated database plus a working directory to run `ergon` in. `claude`
/// is always replaced by a fake that records its directory and arguments, so
/// tests can never start a real session.
struct Env {
    home: TempDir,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fake = home.path().join("fake-claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" > \"$ERGON_FAKE_LOG\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        Env { home }
    }

    /// The fake claude's last invocation: its directory, then its arguments.
    fn launched(&self) -> Vec<String> {
        let log = self.home.path().join("claude.log");
        let lines = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_file(&log).unwrap();
        lines.lines().map(String::from).collect()
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
            .env("ERGON_CLAUDE", self.home.path().join("fake-claude"))
            .env("ERGON_FAKE_LOG", self.home.path().join("claude.log"))
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

    env.ok(&a, &["init", "alpha", "Alpha intent", "--no-launch"]);
    env.ok(&b, &["init", "beta", "Beta intent", "--no-launch"]);
    assert!(a.join(".ergon").is_file());
    assert!(
        env.err(&a, &["init", "again", "x", "--no-launch"])
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
    env.ok(&dir, &["init", "demo", "Root intent", "--no-launch"]);

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
        tree.contains(&format!("    {b}  todo         p1  API")),
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
    env.ok(&dir, &["init", "demo", "Root", "--no-launch"]);
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
    assert!(
        env.err(&dir, &["dep", "rm", &a, &b])
            .contains("doesn't depend")
    );
}

#[test]
fn ls_command() {
    let env = Env::new();
    let dir = env.dir("proj");
    env.ok(&dir, &["init", "demo", "Root", "--no-launch"]);
    let a = env
        .ok(&dir, &["add", "Parent", "--priority", "p2"])
        .trim()
        .to_string();
    let a1 = env
        .ok(&dir, &["add", "First", "--parent", &a])
        .trim()
        .to_string();
    let a2 = env
        .ok(&dir, &["add", "Second", "--parent", &a])
        .trim()
        .to_string();
    let x = env.ok(&dir, &["add", "Prerequisite"]).trim().to_string();

    let ls = env.ok(&dir, &["ls"]);
    assert_eq!(
        ls,
        format!("{a}  p2  todo         Parent\n{x}  p-  todo         Prerequisite\n")
    );

    env.ok(&dir, &["dep", "add", &a2, &x]);
    // Both are p2 (x is boosted by a2); a1 is older.
    let ls = env.ok(&dir, &["ls"]);
    assert_eq!(
        ls,
        format!("{a1}  p2  todo         First  (in: Parent)\n{x}  p2  todo         Prerequisite\n")
    );
    let show = env.ok(&dir, &["show", &a2]);
    assert!(
        show.contains(&format!("blocked:   yes, waiting on demo/{x}")),
        "{show}"
    );
    let show = env.ok(&dir, &["show", &x]);
    assert!(show.contains("priority:  p2 (inferred)"), "{show}");
}

#[test]
fn rm_and_restore() {
    let env = Env::new();
    let dir = env.dir("proj");
    env.ok(&dir, &["init", "demo", "Root", "--no-launch"]);
    let a = env.ok(&dir, &["add", "A"]).trim().to_string();
    env.ok(&dir, &["add", "A1", "--parent", &a]);
    let x = env.ok(&dir, &["add", "X"]).trim().to_string();
    env.ok(&dir, &["dep", "add", &x, &a]);

    let show = env.ok(&dir, &["show", &a]);
    let line = show.lines().find(|l| l.starts_with("delete:")).unwrap();
    assert_eq!(
        line,
        format!("delete:    ergon rm demo/{a} --subtasks 1 --dependents 1")
    );

    let err = env.err(&dir, &["rm", &a, "--subtasks", "0", "--dependents", "0"]);
    assert!(err.contains("--subtasks 1 --dependents 1"), "{err}");
    let args: Vec<&str> = line.split_whitespace().skip(2).collect();
    env.ok(&dir, &args);

    assert!(env.err(&dir, &["show", &a]).contains("is deleted"));
    assert!(!env.ok(&dir, &["tree"]).contains(" A"));
    assert!(env.ok(&dir, &["show", &x]).contains("blocked:   no"));

    let out = env.ok(&dir, &["restore", &a]);
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(env.ok(&dir, &["show", &x]).contains("blocked:   yes"));
}

#[test]
fn work_sessions() {
    let env = Env::new();
    let dir = env.dir("proj");
    let other = env.dir("other");
    let dir_s = dir.to_str().unwrap().to_string();

    // init launches the planner with a fresh session and the planner prompt.
    env.ok(&dir, &["init", "demo", "Build a demo"]);
    let l = env.launched();
    assert_eq!(l[0], dir_s);
    assert_eq!(l[1], "--session-id");
    let root_session = l[2].clone();
    let prompt = l[3..].join("\n");
    assert!(prompt.starts_with("# You are the planner"), "{prompt}");
    assert!(prompt.contains("Project: demo") && prompt.contains("Build a demo"), "{prompt}");
    let show = env.ok(&dir, &["show", "root"]);
    assert!(show.contains(&format!("session:   {root_session} in {dir_s}")), "{show}");

    // Resuming happens in the recorded directory, wherever ergon runs.
    env.ok(&other, &["work", "demo/root"]);
    assert_eq!(env.launched(), [dir_s.clone(), "--resume".into(), root_session]);

    // A first work session records the cwd and starts the task.
    let a = env.ok(&dir, &["add", "Backend"]).trim().to_string();
    let b = env.ok(&dir, &["add", "API", "--parent", &a]).trim().to_string();
    let other_s = other.to_str().unwrap();
    env.ok(&other, &["work", &format!("demo/{b}")]);
    let l = env.launched();
    assert_eq!((l[0].as_str(), l[1].as_str()), (other_s, "--session-id"));
    let prompt = l[3..].join("\n");
    assert!(prompt.contains(&format!("Task to complete here (demo/{b}):")), "{prompt}");
    let show = env.ok(&dir, &["show", &b]);
    assert!(show.contains("status:    in_progress\n"), "{show}");
    assert!(show.contains(&format!(" in {other_s}")), "{show}");
    // a was already todo -> its child's work doesn't touch it.
    assert!(env.ok(&dir, &["show", &a]).contains("status:    todo"));

    // Closed tasks can't be worked; blocked ones warn.
    let done = env.ok(&dir, &["add", "Done", "--status", "done"]).trim().to_string();
    assert!(env.err(&dir, &["work", &done]).contains("is done"));
    let blocked = env.ok(&dir, &["add", "Blocked"]).trim().to_string();
    env.ok(&dir, &["dep", "add", &blocked, &a]);
    let out = env.run_in(&dir, &["work", &blocked]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is blocked"));
    env.launched();

    // A vanished session directory is an error.
    std::fs::remove_dir(&other).unwrap();
    assert!(env.err(&dir, &["work", &b]).contains("no longer exists"));
}

#[test]
fn help_topics() {
    let env = Env::new();
    let dir = env.dir("x");
    assert!(env.ok(&dir, &["help", "planner"]).starts_with("# You are the planner"));
    assert!(env.ok(&dir, &["help"]).contains("Usage: ergon"));
    assert!(env.ok(&dir, &["help", "rm"]).contains("--subtasks"));
    assert!(env.err(&dir, &["help", "nope"]).contains("no help topic"));
}
