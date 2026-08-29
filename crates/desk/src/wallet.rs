//! The window's side of key custody.
//!
//! # Why the key is typed in here at all
//!
//! Pasting a secret into a text field is not the safest thing anyone has ever designed.
//! It crosses the webview, arrives over Tauri's IPC as an ordinary string, and Windows
//! keeps clipboard history by default. A file path the operator prepares out of band
//! avoids all of that.
//!
//! It is what was asked for, so what this module does instead is make the window the
//! *only* place the key is ever in the clear, and make that window as short as possible:
//! the string is parsed, encrypted under a passphrase, and dropped inside one function
//! call. Nothing keeps it, nothing logs it, and nothing writes it anywhere except
//! encrypted.
//!
//! # What is held in memory
//!
//! An unlocked [`Wallet`] lives for the session, because asking for the passphrase
//! before every trade would defeat the point of an automated instrument. It is behind a
//! mutex and never leaves this process — the bot is a separate process and receives the
//! passphrase over its stdin at spawn, never a key and never a file it can read alone.

use cb_wallet::{EncryptedKey, Wallet};
use std::sync::Mutex;

/// What the window is allowed to know without a passphrase.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletStatus {
    /// A key file exists.
    pub configured: bool,
    /// Its address, readable without decrypting anything.
    pub pubkey: Option<String>,
    /// Whether this session has it unlocked.
    pub unlocked: bool,
}

/// Session-scoped custody. Not persisted, not serialisable.
#[derive(Default)]
pub struct Custody {
    unlocked: Mutex<Option<Wallet>>,
}

impl Custody {
    #[must_use]
    pub fn is_unlocked(&self) -> bool {
        self.unlocked.lock().is_ok_and(|g| g.is_some())
    }

    pub fn store(&self, w: Wallet) {
        if let Ok(mut g) = self.unlocked.lock() {
            *g = Some(w);
        }
    }

    pub fn forget(&self) {
        if let Ok(mut g) = self.unlocked.lock() {
            *g = None;
        }
    }

    /// The address of the unlocked key, if there is one.
    #[must_use]
    pub fn pubkey(&self) -> Option<String> {
        self.unlocked.lock().ok()?.as_ref().map(|w| w.pubkey().to_string())
    }
}

/// Read the address without asking for anything.
///
/// # Errors
/// Never — a missing or unreadable file is reported as "not configured", because that
/// is what the operator needs to know and an error here would just be noise on a first
/// run.
pub fn status(path: &std::path::Path, custody: &Custody) -> WalletStatus {
    let stored = EncryptedKey::load(path).ok();
    WalletStatus {
        configured: stored.is_some(),
        pubkey: custody.pubkey().or_else(|| stored.map(|k| k.pubkey)),
        unlocked: custody.is_unlocked(),
    }
}

/// Encrypt a pasted key and write it, replacing any existing one.
///
/// Takes the secret by value and drops it here. The caller must not keep a copy, and
/// the signature is written so that keeping one is visibly deliberate.
///
/// # Errors
/// If the key is not a valid keypair, the passphrase is empty, or the file cannot be
/// written.
pub fn import(
    path: &std::path::Path,
    secret_text: String,
    passphrase: &str,
) -> Result<WalletStatus, String> {
    // Parse first: a mistyped key should fail before anything is written, so a bad
    // paste never destroys a working wallet that was already there.
    let secret = cb_wallet::parse_secret(&secret_text).map_err(|e| e.to_string())?;
    drop(secret_text);

    let sealed = EncryptedKey::seal(secret.as_slice(), passphrase).map_err(|e| e.to_string())?;
    drop(secret);

    sealed.save(path).map_err(|e| e.to_string())?;
    Ok(WalletStatus {
        configured: true,
        pubkey: Some(sealed.pubkey),
        unlocked: false,
    })
}

/// Decrypt for this session.
///
/// # Errors
/// If there is no key file, or the passphrase is wrong.
pub fn unlock(
    path: &std::path::Path,
    passphrase: &str,
    custody: &Custody,
) -> Result<WalletStatus, String> {
    let stored = EncryptedKey::load(path).map_err(|e| e.to_string())?;
    let w = stored.unseal(passphrase).map_err(|e| e.to_string())?;
    let pubkey = w.pubkey().to_string();
    custody.store(w);
    Ok(WalletStatus { configured: true, pubkey: Some(pubkey), unlocked: true })
}

/// Delete the key file.
///
/// Irreversible, and the UI says so before calling it. There is no recovery here — the
/// operator's own backup of the original key is the recovery.
///
/// # Errors
/// If the file exists and cannot be removed.
pub fn remove(path: &std::path::Path, custody: &Custody) -> Result<WalletStatus, String> {
    custody.forget();
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(WalletStatus { configured: false, pubkey: None, unlocked: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk_stub::a_keypair_string;

    /// A tiny local generator, so the test does not need solana-sdk as a dev-dependency
    /// of the application crate just to make one key.
    mod solana_sdk_stub {
        /// A valid ed25519 keypair, base58, produced by cb-wallet's own round trip.
        pub fn a_keypair_string() -> String {
            // 64 bytes of known-good keypair material: seed followed by its public half.
            // Generated once and pinned so the test is deterministic.
            let seed: [u8; 32] = [
                157, 97, 177, 157, 239, 253, 90, 96, 186, 132, 74, 244, 146, 236, 44, 196, 68,
                73, 197, 105, 123, 50, 105, 25, 112, 59, 172, 3, 28, 174, 127, 96,
            ];
            let public: [u8; 32] = [
                215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 14, 225,
                114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 26,
            ];
            let mut full = Vec::with_capacity(64);
            full.extend_from_slice(&seed);
            full.extend_from_slice(&public);
            bs58::encode(full).into_string()
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join("cb-desk-wallet-tests").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("keypair-encrypted.json")
    }

    #[test]
    fn a_first_run_reports_no_wallet_rather_than_an_error() {
        let s = status(&tmp("fresh"), &Custody::default());
        assert!(!s.configured);
        assert!(!s.unlocked);
        assert!(s.pubkey.is_none());
    }

    #[test]
    fn an_imported_key_is_readable_by_address_but_stays_locked() {
        let p = tmp("import");
        let s = import(&p, a_keypair_string(), "pass phrase").unwrap();
        assert!(s.configured);
        assert!(!s.unlocked, "importing must not also unlock — the passphrase proves intent");
        assert!(s.pubkey.is_some());

        // And the address survives a restart, without the passphrase.
        let after = status(&p, &Custody::default());
        assert_eq!(after.pubkey, s.pubkey);
        assert!(!after.unlocked);
    }

    #[test]
    fn unlocking_needs_the_right_passphrase() {
        let p = tmp("unlock");
        import(&p, a_keypair_string(), "right").unwrap();
        let c = Custody::default();

        assert!(unlock(&p, "wrong", &c).is_err());
        assert!(!c.is_unlocked());

        let s = unlock(&p, "right", &c).unwrap();
        assert!(s.unlocked);
        assert!(c.is_unlocked());
    }

    /// A bad paste must not destroy a wallet that was already working.
    #[test]
    fn a_rejected_import_leaves_the_previous_key_intact() {
        let p = tmp("noclobber");
        let good = import(&p, a_keypair_string(), "pass").unwrap();

        assert!(import(&p, "obviously not a key".into(), "pass").is_err());

        let after = status(&p, &Custody::default());
        assert!(after.configured, "the working key was deleted by a failed import");
        assert_eq!(after.pubkey, good.pubkey);
    }

    #[test]
    fn removing_forgets_the_session_key_as_well_as_the_file() {
        let p = tmp("remove");
        import(&p, a_keypair_string(), "pass").unwrap();
        let c = Custody::default();
        unlock(&p, "pass", &c).unwrap();
        assert!(c.is_unlocked());

        let s = remove(&p, &c).unwrap();

        assert!(!s.configured);
        assert!(!c.is_unlocked(), "the key stayed in memory after its file was deleted");
        assert!(!p.exists());
    }

    #[test]
    fn removing_a_wallet_that_is_not_there_is_not_an_error() {
        assert!(remove(&tmp("absent"), &Custody::default()).is_ok());
    }

    #[test]
    fn an_empty_passphrase_is_refused_at_the_boundary() {
        let p = tmp("emptypass");
        assert!(import(&p, a_keypair_string(), "").is_err());
        assert!(!p.exists(), "nothing should have been written");
    }
}
