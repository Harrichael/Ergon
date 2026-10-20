//! Scenario tests. Each file in `tests/scenarios/` is a transcript: commands
//! and the exact output they must produce.
//!
//! ```text
//! # Comments start with '#'.
//! $ ergon add "Parent" --priority p2
//! Hc4q
//! $ ergon edit Hc4q --status nope
//! error: invalid value 'nope' for '--status <STATUS>': ...
//! ...
//! [exit 2]
//! ```
//!
//! - `$ ergon ...` runs ergon. Arguments are split like a shell: single and
//!   double quotes group, and `\n` inside double quotes is a newline. Leading
//!   `NAME=value` words set environment variables for that command only.
//! - A command ending in `<<EOF` reads the lines up to `EOF` as its stdin,
//!   and one ending in `> <file>` writes its stdout to that file instead.
//! - `$ cd <dir>` changes directory (created if missing) and `$ rmdir <dir>`
//!   removes one; both are relative to the scenario's root.
//! - The expected output is every line up to the next `$ ` line, minus
//!   trailing blank and comment lines. It's stdout, then stderr, then
//!   `[exit N]` if the command failed. A line of just `...` matches any
//!   number of lines. The scenario root is shown as `$ROOT`.
//!
//! Every scenario gets a fresh database, `ERGON_SEED=1` (so slugs and UUIDs
//! are deterministic), a frozen clock (`ERGON_NOW`, shown in UTC), no colour,
//! and a fake `claude` that prints its directory and arguments.
//!
//! Run with `ERGON_BLESS=1` to rewrite failing expectations with the actual
//! output, and `ERGON_SCENARIO=<name>` to run only matching files.

use std::path::{Path, PathBuf};
use std::process::Command;

const NOW: &str = "1760000000"; // 2025-10-09 08:53:20 UTC

const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '[claude in %s]\n' "$PWD"
for arg in "$@"; do printf '%s\n' "$arg"; done
"#;

struct Step {
    /// Lines before the command: comments and blank lines.
    preamble: Vec<String>,
    command: String,
    stdin: Option<Vec<String>>,
    expected: Vec<String>,
}

fn parse(text: &str) -> (Vec<String>, Vec<Step>) {
    let mut header = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut pending = Vec::new();
    let mut in_stdin = false;
    for line in text.lines() {
        if in_stdin {
            let stdin = steps.last_mut().unwrap().stdin.as_mut().unwrap();
            if line == "EOF" {
                in_stdin = false;
            } else {
                stdin.push(line.to_string());
            }
        } else if let Some(cmd) = line.strip_prefix("$ ") {
            if let Some(last) = steps.last_mut() {
                // Trailing blank and comment lines belong to the next step.
                while last
                    .expected
                    .last()
                    .is_some_and(|l| l.trim().is_empty() || l.starts_with('#'))
                {
                    pending.insert(0, last.expected.pop().unwrap());
                }
            } else {
                header.append(&mut pending);
            }
            let (command, stdin) = match cmd.strip_suffix(" <<EOF") {
                Some(c) => (c, Some(Vec::new())),
                None => (cmd, None),
            };
            in_stdin = stdin.is_some();
            steps.push(Step {
                preamble: std::mem::take(&mut pending),
                command: command.to_string(),
                stdin,
                expected: Vec::new(),
            });
        } else if let Some(last) = steps.last_mut() {
            last.expected.push(line.to_string());
        } else {
            pending.push(line.to_string());
        }
    }
    header.append(&mut pending);
    if let Some(last) = steps.last_mut() {
        while last.expected.last().is_some_and(|l| l.trim().is_empty()) {
            last.expected.pop();
        }
    }
    (header, steps)
}

/// Splits a command line into words, shell style.
fn split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                word.extend(chars.by_ref().take_while(|&c| c != '\''));
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => word.push('\n'),
                            Some(other) => word.push(other),
                            None => {}
                        },
                        _ => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Whether `actual` matches `expected`, where a `...` line matches any run
/// of lines.
fn matches(expected: &[String], actual: &[String]) -> bool {
    match expected.split_first() {
        None => actual.is_empty(),
        Some((e, rest)) if e == "..." => (0..=actual.len()).any(|i| matches(rest, &actual[i..])),
        Some((e, rest)) => actual
            .split_first()
            .is_some_and(|(a, arest)| a == e && matches(rest, arest)),
    }
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    claude: PathBuf,
    cwd: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let claude = tmp.path().join("fake-claude");
        std::fs::write(&claude, FAKE_CLAUDE).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        Sandbox {
            db: tmp.path().join("ergon.db"),
            cwd: root.clone(),
            root,
            claude,
            _tmp: tmp,
        }
    }

    fn run(&mut self, command: &str, stdin: Option<&[String]>) -> Vec<String> {
        let mut words = split(command);
        let mut env = Vec::new();
        while let Some((k, v)) = words.first().and_then(|w| w.split_once('=')) {
            env.push((k.to_string(), v.to_string()));
            words.remove(0);
        }
        let redirect = match words.iter().position(|w| w == ">") {
            Some(i) => {
                let file = self.cwd.join(&words[i + 1]);
                words.truncate(i);
                Some(file)
            }
            None => None,
        };
        let (program, args) = words.split_first().expect("empty command");
        match program.as_str() {
            "cd" => {
                self.cwd = self.root.join(&args[0]);
                std::fs::create_dir_all(&self.cwd).unwrap();
                Vec::new()
            }
            "rmdir" => {
                std::fs::remove_dir_all(self.root.join(&args[0])).unwrap();
                Vec::new()
            }
            "ergon" => self.ergon(args, &env, stdin, redirect),
            other => panic!("unknown scenario command {other:?}"),
        }
    }

    fn ergon(
        &self,
        args: &[String],
        env: &[(String, String)],
        stdin: Option<&[String]>,
        redirect: Option<PathBuf>,
    ) -> Vec<String> {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new(env!("CARGO_BIN_EXE_ergon"))
            .args(args)
            .current_dir(&self.cwd)
            .env("ERGON_DB", &self.db)
            .env("ERGON_CLAUDE", &self.claude)
            .env("ERGON_SEED", "1")
            .env("ERGON_NOW", NOW)
            .env("TZ", "UTC")
            .env("NO_COLOR", "1")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input: String = stdin
            .into_iter()
            .flatten()
            .map(|l| format!("{l}\n"))
            .collect();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        if let Some(file) = redirect {
            std::fs::write(file, std::mem::take(&mut text)).unwrap();
        }
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            text.push_str(&format!("[exit {}]\n", out.status.code().unwrap_or(-1)));
        }
        let text = text.replace(self.root.to_str().unwrap(), "$ROOT");
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines
    }
}

/// Runs one scenario, returning a failure report if any step mismatched.
fn run_scenario(path: &Path, bless: bool) -> Option<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let (header, mut steps) = parse(&text);
    let mut sandbox = Sandbox::new();
    let mut failure = None;
    for step in &mut steps {
        let actual = sandbox.run(&step.command, step.stdin.as_deref());
        if matches(&step.expected, &actual) {
            continue;
        }
        if failure.is_none() {
            failure = Some(format!(
                "{}\n$ {}\n--- expected\n{}\n--- actual\n{}\n",
                path.display(),
                step.command,
                step.expected.join("\n"),
                actual.join("\n")
            ));
        }
        if !bless {
            break;
        }
        step.expected = actual;
    }
    if bless && failure.is_some() {
        let mut out = header.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        for step in &steps {
            for line in step.preamble.iter().chain([&format!("$ {}", step.command)]) {
                out.push_str(line);
                out.push('\n');
            }
            if let Some(stdin) = &step.stdin {
                out.pop();
                out.push_str(" <<EOF\n");
                for line in stdin.iter().chain([&"EOF".to_string()]) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            for line in &step.expected {
                out.push_str(line);
                out.push('\n');
            }
        }
        std::fs::write(path, out).unwrap();
    }
    failure
}

#[test]
fn scenarios() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let filter = std::env::var("ERGON_SCENARIO").ok();
    let bless = std::env::var_os("ERGON_BLESS").is_some();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .filter(|p| {
            filter
                .as_ref()
                .is_none_or(|f| p.to_string_lossy().contains(f))
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no scenarios found in {}", dir.display());

    let failures: Vec<String> = paths
        .iter()
        .filter_map(|p| run_scenario(p, bless))
        .collect();
    if !failures.is_empty() {
        let verb = if bless { "blessed" } else { "failed" };
        panic!(
            "{} scenario(s) {verb}:\n\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

#[test]
fn runner_parsing() {
    assert_eq!(
        split(r#"ergon add "Two\nlines" --x 'a b' c\ d"#),
        ["ergon", "add", "Two\nlines", "--x", "a b", "c d"]
    );
    let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
    assert!(matches(&lines("a\n...\nd"), &lines("a\nb\nc\nd")));
    assert!(matches(&lines("a\n..."), &lines("a")));
    assert!(!matches(&lines("a\n...\nd"), &lines("a\nb")));

    let (header, steps) = parse("# intro\n\n$ ergon x\nout\n\n# next\n$ ergon y\n");
    assert_eq!(header, ["# intro", ""]);
    assert_eq!(steps[0].expected, ["out"]);
    assert_eq!(steps[1].preamble, ["", "# next"]);

    let (_, steps) = parse("$ ergon x <<EOF\n# in\n$ not a command\nEOF\nout\n");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].command, "ergon x");
    assert_eq!(
        steps[0].stdin.as_deref().unwrap(),
        ["# in", "$ not a command"]
    );
    assert_eq!(steps[0].expected, ["out"]);
}
