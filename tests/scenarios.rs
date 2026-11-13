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
//! - `$ fake-choros <<EOF` puts a fake `choros` on PATH for the rest of the
//!   scenario, with this directory a trunk of its (`trunks` lists them)
//!   whose landings are now the ones in the JSON given here, written as
//!   `choros status --json` writes them. Its `events` tells of each, and of
//!   a set no longer among them as dropped; `ergon sync` reads that. Its
//!   `here` names this directory as a workspace, and fails elsewhere.
//!   `here --json` calls that directory a workspace too, or the kind in
//!   `CHOROS_KIND`. Its `landing`, `hook` and `work` print the directory
//!   and arguments they were run with, and `work` makes the folder it names.
//!   From then on the scenario also stands in for a running `ergond`, as
//!   far as holding its lock goes, until `$ stop-ergond`.
//! - The expected output is every line up to the next `$ ` line, minus
//!   trailing blank and comment lines. It's stdout, then stderr, then
//!   `[exit N]` if the command failed. A line of just `...` matches any
//!   number of lines. The scenario root is shown as `$ROOT`.
//!
//! Every scenario gets a fresh database, `ERGON_SEED=1` (so slugs and UUIDs
//! are deterministic), a frozen clock (`ERGON_NOW`, shown in UTC), no colour,
//! `PATH=/usr/bin:/bin`, and a fake `claude` that prints its directory and
//! arguments and saves the conversation, with the directory it began in,
//! under its own `CLAUDE_CONFIG_DIR` (not if `CRASH` is set).
//!
//! Run with `ERGON_BLESS=1` to rewrite failing expectations with the actual
//! output, and `ERGON_SCENARIO=<name>` to run only matching files.

use std::path::{Path, PathBuf};
use std::process::Command;

const NOW: &str = "1760000000"; // 2025-10-09 08:53:20 UTC

// Saves the conversation where Claude Code would, unless CRASH is set.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '[claude in %s]\n' "$PWD"
for arg in "$@"; do printf '%s\n' "$arg"; done
if [ "$1" = --session-id ] && [ -z "$CRASH" ]; then
  mkdir -p "$CLAUDE_CONFIG_DIR/projects/p" && printf '{"cwd":"%s"}\n' "$PWD" > "$CLAUDE_CONFIG_DIR/projects/p/$2.jsonl"
fi
"#;

// Treats the directory `fake-choros` left its JSON in as a workspace, and
// as a trunk whose event log is the file beside it.
const FAKE_CHOROS: &str = r#"#!/bin/sh
case "$1" in
  landing) printf '[choros in %s]\n' "$PWD"; printf '%s\n' "$@"; exit 0;;
  hook) printf '%s' "$3" > .choros-hook; printf '[choros in %s]\n' "$PWD"; printf '%s\n' "$@"; exit 0;;
  work)
    printf '[choros in %s]\n' "$PWD" >&2; printf '%s\n' "$@" >&2
    if [ "$2" = -i ]; then
      shift
      read -r picked || { echo "choros: Failed to initialize input reader" >&2; exit 1; }
      printf 'picked: %s\n' "$picked" >&2
    fi
    mkdir "$2" && cp .choros-status.json "$2" && printf '%s\n' "$PWD/$2"; exit;;
esac
if [ "$*" = "here --json" ]; then
  [ -f .choros-status.json ] || exit 1
  if [ -f .choros-hook ]; then
    printf '{"kind": "workspace", "land_hook": "%s"}\n' "$(cat .choros-hook)"
  else
    printf '{"kind": "%s", "land_hook": null}\n' "${CHOROS_KIND:-workspace}"
  fi
  exit 0
fi
if [ "$1" = trunks ]; then cat "$0.trunks"; exit 0; fi
case "$*" in "events --since "*|here) ;; *) echo "choros: fake choros can't $*" >&2; exit 2;; esac
[ -f .choros-status.json ] || { echo "choros: not under a choros trunk (no .choros-config above here)" >&2; exit 1; }
if [ "$1" = here ]; then printf 'here: workspace %s\n' "${PWD##*/}"; else tail -n +$(($3 + 1)) .choros-events.jsonl; fi
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
    /// Holds the fake `choros` once a scenario asks for it.
    bin: Option<PathBuf>,
    /// The lock a running `ergond` holds.
    ergond: Option<std::fs::File>,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        // ergon prints the directory it runs in, which has symlinks resolved
        // (on macOS the temp dir is under /var, a link to /private/var).
        let root = root.canonicalize().unwrap();
        let claude = tmp.path().join("fake-claude");
        write_script(&claude, FAKE_CLAUDE);
        Sandbox {
            db: tmp.path().join("ergon.db"),
            cwd: root.clone(),
            root,
            claude,
            bin: None,
            ergond: None,
            _tmp: tmp,
        }
    }

    fn fake_choros(&mut self, status: &[String]) {
        use serde_json::{Value, json};
        let bin = self.claude.with_file_name("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("choros"), FAKE_CHOROS);
        self.bin = Some(bin.clone());
        if self.ergond.is_none() {
            self.ergond = ergon::sync::lock(&self.db).unwrap();
        }

        let trunks = bin.join("choros.trunks");
        let mut listed = std::fs::read_to_string(&trunks).unwrap_or_default();
        let line = format!("{}\n", self.cwd.display());
        if !listed.contains(&line) {
            listed.push_str(&line);
            std::fs::write(&trunks, listed).unwrap();
        }

        // A set's key, with its event.
        let events = |status: &str| -> Vec<(String, Value)> {
            let status: Value = serde_json::from_str(status).expect("fake-choros takes JSON");
            let mut out = Vec::new();
            for q in status["queues"].as_array().into_iter().flatten() {
                for set in q["sets"].as_array().into_iter().flatten() {
                    out.push((
                        format!("{}/{}", q["name"], set["n"]),
                        json!({"queue": q["name"], "set": set}),
                    ));
                }
            }
            for i in status["instant"].as_array().into_iter().flatten() {
                out.push((String::new(), json!({"instant": i})));
            }
            out
        };
        let status_file = self.cwd.join(".choros-status.json");
        let status = status.join("\n");
        let mut told = events(&status);
        if let Ok(before) = std::fs::read_to_string(&status_file) {
            for (key, mut event) in events(&before) {
                if !key.is_empty() && !told.iter().any(|(k, _)| *k == key) {
                    event["set"]["state"] = json!("dropped");
                    told.push((key, event));
                }
            }
        }
        std::fs::write(&status_file, status).unwrap();

        let log = self.cwd.join(".choros-events.jsonl");
        let mut lines = std::fs::read_to_string(&log).unwrap_or_default();
        let mut seq = lines.lines().count();
        for (_, mut event) in told {
            seq += 1;
            event["seq"] = json!(seq);
            event["trunk"] = json!(self.cwd);
            lines.push_str(&format!("{event}\n"));
        }
        std::fs::write(&log, lines).unwrap();
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
            "stop-ergond" => {
                self.ergond = None;
                Vec::new()
            }
            "fake-choros" => {
                self.fake_choros(stdin.expect("fake-choros reads its JSON from <<EOF"));
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
            .env(
                "CLAUDE_CONFIG_DIR",
                self.claude.with_file_name("claude-config"),
            )
            .env("ERGON_SEED", "1")
            .env("ERGON_NOW", NOW)
            .env("TZ", "UTC")
            .env("NO_COLOR", "1")
            // The work prompt says more when choros is on PATH; keep the
            // machine's install out of it.
            .env(
                "PATH",
                match &self.bin {
                    Some(bin) => format!("{}:/usr/bin:/bin", bin.display()),
                    None => "/usr/bin:/bin".to_string(),
                },
            )
            // The tests may themselves run inside Claude Code.
            .env_remove("CLAUDE_CODE_SESSION_ID")
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

fn write_script(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
