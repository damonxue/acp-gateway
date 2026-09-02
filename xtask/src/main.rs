//! Build tasks, reachable through the cargo aliases in `.cargo/config.toml`.
//!
//! Cargo aliases can only expand to cargo subcommands, so "run the packaging
//! script" needs one indirection. This crate is that indirection and nothing
//! more: it has no dependencies, so `cargo dmg` compiles in under a second and
//! the real logic stays in `script/bundle-mac.sh` where it is readable as a
//! sequence of shell commands.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let task = arguments.next().unwrap_or_default();
    let rest: Vec<String> = arguments.collect();

    match task.as_str() {
        "dmg" => run_script("bundle-mac.sh", &rest),
        "" | "help" | "-h" | "--help" => {
            usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown task `{other}`\n");
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "cargo xtask <task>\n\
         \n\
         tasks:\n\
         \x20 dmg [--no-app] [--no-web] [--sign] [--open]   build the .app and .dmg\n\
         \n\
         aliases:\n\
         \x20 cargo dmg        \x20 cargo dmg-cli        \x20 cargo app"
    );
}

fn run_script(name: &str, arguments: &[String]) -> ExitCode {
    let script = workspace_root().join("script").join(name);
    if !script.exists() {
        eprintln!("missing {}", script.display());
        return ExitCode::FAILURE;
    }

    // Inherit stdio: the script's progress is the task's output.
    let status = Command::new(&script)
        .args(arguments)
        .status()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", script.display()));

    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The workspace root, derived from this crate's own location.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}
