//! Where the project is, resolved once.
//!
//! One value — the project root — and every other path derived from it. The
//! alternative, letting each module find `config.toml` its own way, is how a config
//! editor and a bot end up disagreeing about which file is the config.
//!
//! # Two shapes of install
//!
//! Run from a checkout, the root is the repository: the config beside `crates/` is the
//! one being edited, and the ledger lands where `cb-bot --report` will look for it. Run
//! from the installer there is no repository at all, so the root becomes a per-user
//! data directory seeded with the default config on first launch. Both resolve through
//! [`Paths::discover`], and a saved choice overrides both.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The default config, compiled in rather than shipped as a bundle resource.
///
/// An installed copy has no repository beside it to copy from, and a resource path is
/// one more thing that can resolve differently on someone else's machine. This cannot
/// go missing.
const DEFAULT_CONFIG: &str = include_str!("../../../config.example.toml");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// Saved choice if it still points at a project, else the checkout we are inside,
    /// else the per-user data directory.
    ///
    /// The last case cannot fail, so a fresh install always has somewhere to record to.
    /// [`Paths::ensure_ready`] is what makes that somewhere usable.
    #[must_use]
    pub fn discover() -> Self {
        if let Some(saved) = Self::load_saved() {
            if saved.config().exists() {
                return saved;
            }
        }
        Self::find_checkout().unwrap_or_else(|| Self { root: Self::data_dir() })
    }

    /// Walk up from the executable and the working directory looking for a repository.
    ///
    /// Both markers are required. `config.toml` alone also describes the data
    /// directory, and matching that here would collapse the distinction this function
    /// exists to draw.
    fn find_checkout() -> Option<Self> {
        let beside_exe =
            std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
        for cand in [beside_exe, std::env::current_dir().ok()].into_iter().flatten() {
            let mut dir = Some(cand);
            while let Some(d) = dir {
                if d.join("config.toml").exists() && d.join("crates").is_dir() {
                    return Some(Self { root: d });
                }
                dir = d.parent().map(Path::to_path_buf);
            }
        }
        None
    }

    /// Where an installed copy keeps its config, ledger and archives.
    ///
    /// Deliberately not beside the executable: the installer puts that under Program
    /// Files, which the user running the app cannot write to, and a ledger that cannot
    /// be opened for writing is a run that records nothing while looking healthy.
    #[must_use]
    pub fn data_dir() -> PathBuf {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        PathBuf::from(local).join("cryptobot")
    }

    /// Create the root, and seed a config if there is not one already.
    ///
    /// Without this a fresh install resolves a config path that does not exist, and
    /// every panel in the window reads as an error with no action attached to it.
    ///
    /// # Errors
    /// If the root cannot be created or the seed config cannot be written.
    pub fn ensure_ready(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        if !self.config().exists() {
            // The seed is the example, so a new install starts in paper mode. Nothing
            // in this application writes `mode`; see the note in `config.rs`.
            std::fs::write(self.config(), DEFAULT_CONFIG)?;
        }
        Ok(())
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

    /// The encrypted signing key.
    ///
    /// Beside the ledger rather than in the settings directory, because it belongs to
    /// the project the operator is running: pointing the app at a different root should
    /// change which wallet is in use, not silently keep the old one. Its name matches
    /// the `keypair*.json` pattern `.gitignore` has excluded since the first commit,
    /// which is deliberate belt-and-braces — the file is encrypted, and also cannot be
    /// committed by accident.
    #[must_use]
    pub fn wallet(&self) -> PathBuf {
        self.root.join("keypair-encrypted.json")
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

    /// The installed case: no repository anywhere on the machine, and the app still
    /// has to have something to read.
    #[test]
    fn a_fresh_root_is_given_a_config_to_start_from() {
        let tmp = std::env::temp_dir().join("cb-paths-seed-test");
        let _ = std::fs::remove_dir_all(&tmp);
        let p = Paths { root: tmp.clone() };

        p.ensure_ready().expect("a writable temp directory should seed");

        assert!(p.config().exists(), "a fresh install has no config until this writes one");
        let seeded = std::fs::read_to_string(p.config()).unwrap();
        assert!(seeded.contains("mode = \"paper\""), "a new install must start in paper mode");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Seeding twice would silently discard whatever had been edited in between.
    #[test]
    fn seeding_never_overwrites_a_config_that_is_already_there() {
        let tmp = std::env::temp_dir().join("cb-paths-noclobber-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let p = Paths { root: tmp.clone() };
        std::fs::write(p.config(), "capital_usd = 12345.0\n").unwrap();

        p.ensure_ready().unwrap();

        let after = std::fs::read_to_string(p.config()).unwrap();
        assert!(after.contains("12345"), "an existing config must survive a later launch");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The data directory must not read as a checkout, or an installed copy would
    /// start walking up Program Files looking for `crates/`.
    #[test]
    fn the_installed_root_is_not_mistaken_for_a_repository() {
        let d = Paths::data_dir();
        assert!(d.ends_with("cryptobot"));
        assert!(!d.join("crates").exists(), "the data directory is not a source tree");
    }
}
