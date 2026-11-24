//! One-time local setup for contributors.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Point git at `.githooks/` so every commit runs the policy check.
pub(crate) fn install_hooks(root: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "core.hooksPath", ".githooks"])
        .current_dir(root)
        .status()
        .context("running git")?;
    anyhow::ensure!(
        status.success(),
        "git config failed (is this a git repository?)"
    );
    Ok(())
}

/// Install hooks and create the local config when it is missing.
///
/// Safe to run again: an existing `victory.toml` is never overwritten.
pub(crate) fn run(root: &Path) -> Result<()> {
    install_hooks(root)?;
    println!("hooks installed: commits now run `cargo xtask policy --staged`");

    let config = root.join("victory.toml");
    let outcome = if config.try_exists().context("checking victory.toml")? {
        "kept (already present)".to_owned()
    } else {
        // Resolve the content first: a failed lookup must not leave an empty
        // file that later runs would treat as a valid config.
        let (content, note) = if cfg!(windows) {
            let example = root.join("victory.example.toml");
            (
                fs::read(&example).with_context(|| format!("reading {}", example.display()))?,
                "created from victory.example.toml (set your paths)".to_owned(),
            )
        } else {
            let home = std::env::var("HOME").context("HOME is not set")?;
            let text = format!(
                "# Local config for VIctory. Set every path outside the repository.\n\
                 # Environment variables override these values.\n\
                 [paths]\n\
                 game_dir = {}\n\
                 cache_dir = {}\n\
                 data_dir = {}\n\
                 test_titles = []\n",
                quoted(&format!("{home}/victory-data/game")),
                quoted(&format!("{home}/.cache/victory/cache")),
                quoted(&format!("{home}/.local/share/victory/data")),
            );
            (
                text.into_bytes(),
                format!("created with paths under {home} (set your own game_dir)"),
            )
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config)
        {
            Ok(mut file) => {
                file.write_all(&content)
                    .with_context(|| format!("writing {}", config.display()))?;
                note
            }
            // Another writer won the race; keep its file.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                "kept (already present)".to_owned()
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", config.display())),
        }
    };
    println!("victory.toml: {outcome}");

    println!("next: set your paths in victory.toml, then run `cargo test --workspace`");
    Ok(())
}

/// A TOML basic string, with the characters that need escaping handled.
fn quoted(text: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quoted;

    #[test]
    fn quotes_paths_safely() {
        assert_eq!(quoted("/home/user"), "\"/home/user\"");
        assert_eq!(
            quoted("a\"b\\c\nd\te\r\u{1}f"),
            "\"a\\\"b\\\\c\\nd\\te\\r\\u0001f\""
        );
    }
}
