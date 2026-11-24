//! Repository maintenance. Run with `cargo xtask <command>`.

mod policy;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git on Linux and macOS skips a hook that is not executable (with only
    /// a hint), so the policy check would silently never run there.
    #[test]
    fn hooks_are_tracked_executable() {
        let out = policy::git_command(&workspace_root())
            .args(["ls-files", "--stage", "--", ".githooks"])
            .output();
        let Some(out) = out.ok().filter(|o| o.status.success()) else {
            eprintln!("skipped: not a git checkout");
            return;
        };
        let listing = String::from_utf8(out.stdout).unwrap();
        assert!(!listing.is_empty(), ".githooks/ holds no tracked hooks");
        for line in listing.lines() {
            assert!(
                line.starts_with("100755 "),
                "hook not executable in git (git update-index --chmod=+x): {line}"
            );
        }
    }
}
