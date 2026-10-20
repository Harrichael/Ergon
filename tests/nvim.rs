//! Runs the neovim plugin's end-to-end test, `nvim/test.lua`, when `nvim` is
//! installed.

use std::process::Command;

#[test]
fn nvim_plugin() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let out = match Command::new("nvim")
        .args(["--headless", "-u", "NONE", "-l", "nvim/test.lua"])
        .current_dir(dir)
        .env("ERGON_BIN", env!("CARGO_BIN_EXE_ergon"))
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            eprintln!("skipping: can't run nvim: {e}");
            return;
        }
    };
    assert!(
        out.status.success(),
        "nvim/test.lua failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
