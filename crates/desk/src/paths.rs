//! Where the project is, resolved once.
//!
//! One value — the repository root — and every other path derived from it. The
//! alternative, letting each module find `config.toml` its own way, is how a config
//! editor and a bot end up disagreeing about which file is the config.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// Saved choice if it still points at a project, else walk up from the executable
    /// and the working directory looking for one.
    #[must_use]
    pub fn discover() -> Self {
        if let Some(saved) = Self::load_saved() {
            if saved.config().exists() {
                return saved;
            }
        }
        let beside_exe =
            std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
        for cand in [beside_exe, std::env::current_dir().ok()].into_iter().flatten() {
            let mut dir = Some(cand);
            while let Some(d) = dir {
                if d.join("config.toml").exists() && d.join("crates").is_dir() {
                    return Self { root: d };
                }
                dir = d.parent().map(Path::to_path_buf);
            }
        }
        Self { root: std::env::current_dir().unwrap_or_default() }
    }

    #[must_use]
    pub fn config(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    #[must_use]
    pub fn ledger(&self) -> PathBuf {
        self.root.join("cryptobot.db")
    }

    #[must_use]
    pub fn archive_dir(&self) -> PathBuf {
        self.root.join("archive")
    }

    #[must_use]
    pub fn log(&self) -> PathBuf {
        self.root.join("cb-bot.log")
    }

    /// Beside the app first — that is what a shipped install looks like — then the
    /// Windows build tree, which is what a development run looks like.
    #[must_use]
    pub fn bot_exe(&self) -> PathBuf {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.join("cb-bot.exe")))
            .filter(|p| p.exists());
        beside.unwrap_or_else(|| {
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            PathBuf::from(local).join("cryptobot-win-target").join("release").join("cb-bot.exe")
        })
    }

    #[must_use]
    pub fn settings_file() -> PathBuf {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("cryptobot-desk").join("settings.json")
    }

    fn load_saved() -> Option<Self> {
        let text = std::fs::read_to_string(Self::settings_file()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// # Errors
    /// If the settings directory cannot be created or the file cannot be written.
    pub fn save(&self) -> anyhow::Result<()> {
        let f = Self::settings_file();
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(f, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything derives from one value, so there is exactly one thing to get wrong
    /// and exactly one place to fix it.
    #[test]
    fn every_project_path_derives_from_the_single_root() {
        let p = Paths { root: PathBuf::from("D:\\proj") };
        assert!(p.config().ends_with("config.toml"));
        assert!(p.ledger().ends_with("cryptobot.db"));
        assert!(p.config().starts_with("D:\\proj"));
        assert!(p.ledger().starts_with("D:\\proj"));
        assert!(p.archive_dir().starts_with("D:\\proj"));
        assert!(p.log().starts_with("D:\\proj"));
    }

    #[test]
    fn the_bot_binary_is_named_even_when_no_build_exists_yet() {
        let p = Paths { root: PathBuf::from("D:\\proj") };
        assert!(p.bot_exe().to_string_lossy().contains("cb-bot"));
    }

    #[test]
    fn settings_live_outside_the_repository() {
        // Otherwise a fresh clone would inherit someone else's project root.
        let f = Paths::settings_file();
        assert!(f.ends_with("settings.json"));
        assert!(f.to_string_lossy().contains("cryptobot-desk"));
    }
}
