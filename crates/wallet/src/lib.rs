//! Key custody.
//!
//! One job: hold a signing key without ever writing it to disk in the clear, and make
//! the careless mistakes hard to make. The key reaches this crate from the application's
//! setup screen, is encrypted under a passphrase, and is never persisted any other way.
//!
//! # What this deliberately refuses to do
//!
//! There is no `Debug`, `Display`, `Serialize`, or `Clone` on anything holding secret
//! material, and no accessor that hands the secret bytes back out. A key that cannot be
//! printed cannot be printed into a log file by someone adding a `tracing::debug!` in a
//! hurry — which is the way key material actually escapes in practice, not by any of the
//! attacks people design against.
//!
//! # Why a passphrase and not the raw file
//!
//! `solana-keygen` writes an unencrypted JSON array, and that is the format most tools
//! hand you. Storing that as-is means anything that reads the user's profile directory —
//! a backup agent, a sync client, a malicious npm postinstall — walks away with the
//! wallet. Argon2id and ChaCha20-Poly1305 turn that into a file that is useless without
//! something only the operator knows.

use anyhow::{anyhow, bail, Context, Result};
use argon2::Argon2;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

/// Bumped if the KDF or cipher ever changes, so an old file fails loudly rather than
/// being fed to the wrong algorithm and reported as a bad passphrase.
const FORMAT_VERSION: u32 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// An ed25519 secret key is 64 bytes: 32 of seed followed by the 32-byte public key.
const KEYPAIR_LEN: usize = 64;

/// The on-disk form. Contains nothing secret on its own.
#[derive(Serialize, Deserialize)]
pub struct EncryptedKey {
    version: u32,
    /// Recorded so the file stays readable if the defaults are ever tuned upward.
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    #[serde(with = "hex_bytes")]
    salt: Vec<u8>,
    #[serde(with = "hex_bytes")]
    nonce: Vec<u8>,
    #[serde(with = "hex_bytes")]
    ciphertext: Vec<u8>,
    /// Stored in the clear on purpose: the application has to be able to show which
    /// wallet is configured, and confirm a decryption produced the expected key, without
    /// asking for the passphrase first.
    pub pubkey: String,
}

/// Argon2id parameters. 64 MiB and three passes is well past the point where a GPU
/// farm makes a dictionary attack cheap, and still unlocks in well under a second.
const M_COST_KIB: u32 = 65_536;
const T_COST: u32 = 3;
const P_COST: u32 = 1;

fn derive(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let params = argon2::Params::new(m, t, p, Some(KEY_LEN))
        .map_err(|e| anyhow!("argon2 parameters rejected: {e}"))?;
    let a2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    a2.hash_password_into(passphrase.as_bytes(), salt, out.as_mut())
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(out)
}

impl EncryptedKey {
    /// Encrypt raw 64-byte keypair material under a passphrase.
    ///
    /// # Errors
    /// If the material is not a valid ed25519 keypair, or encryption fails.
    pub fn seal(secret: &[u8], passphrase: &str) -> Result<Self> {
        if secret.len() != KEYPAIR_LEN {
            bail!("a Solana keypair is {KEYPAIR_LEN} bytes, got {}", secret.len());
        }
        if passphrase.is_empty() {
            bail!("a passphrase is required — an empty one encrypts nothing");
        }
        // Round-trip through Keypair first: this rejects material whose public half does
        // not match its private half, which is the shape a truncated or mis-pasted key
        // takes. Better to fail here than to store something that can never sign.
        let kp = Keypair::try_from(secret).map_err(|e| anyhow!("not a valid keypair: {e}"))?;
        let pubkey = kp.pubkey().to_string();

        let mut salt = vec![0u8; SALT_LEN];
        let mut nonce_bytes = vec![0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut salt);
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        let dk = derive(passphrase, &salt, M_COST_KIB, T_COST, P_COST)?;
        let cipher = ChaCha20Poly1305::new(dk.as_ref().into());
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), secret)
            .map_err(|e| anyhow!("encryption failed: {e}"))?;

        Ok(Self {
            version: FORMAT_VERSION,
            m_cost: M_COST_KIB,
            t_cost: T_COST,
            p_cost: P_COST,
            salt,
            nonce: nonce_bytes,
            ciphertext,
            pubkey,
        })
    }

    /// # Errors
    /// If the passphrase is wrong, the file is corrupt, or the format is from a future
    /// version of this program.
    pub fn unseal(&self, passphrase: &str) -> Result<Wallet> {
        if self.version != FORMAT_VERSION {
            bail!(
                "key file is format version {} but this build understands {FORMAT_VERSION}",
                self.version
            );
        }
        let dk = derive(passphrase, &self.salt, self.m_cost, self.t_cost, self.p_cost)?;
        let cipher = ChaCha20Poly1305::new(dk.as_ref().into());
        let mut plain = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(&self.nonce), self.ciphertext.as_ref())
                // Poly1305 rejects both a wrong passphrase and a tampered file, and
                // cannot tell them apart. Say the likely one without claiming certainty.
                .map_err(|_| anyhow!("could not decrypt — wrong passphrase, or the file is damaged"))?,
        );
        let kp = Keypair::try_from(plain.as_slice())
            .map_err(|e| anyhow!("decrypted material is not a keypair: {e}"))?;
        plain.zeroize();

        // A file whose recorded pubkey disagrees with what decrypted is a file that has
        // been edited. Refuse it rather than signing with a key the operator has not seen.
        if kp.pubkey().to_string() != self.pubkey {
            bail!("key file is inconsistent: the stored address does not match the key inside it");
        }
        Ok(Wallet { keypair: kp })
    }

    /// # Errors
    /// If the file cannot be read or is not valid JSON.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read key file at {}", path.display()))?;
        serde_json::from_str(&text).context("key file is not valid JSON")
    }

    /// # Errors
    /// If the directory cannot be created or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)
            .with_context(|| format!("could not write key file at {}", path.display()))?;
        restrict_permissions(path);
        Ok(())
    }
}

/// Best-effort tightening of the file's ACL.
///
/// Not load-bearing — the encryption is what protects the key — but a file only the
/// owner can read is one fewer way for it to end up somewhere unexpected.
#[cfg(windows)]
fn restrict_permissions(path: &Path) {
    // icacls is the only reliable way to do this without a Windows API crate, and a
    // failure here is not worth failing the save over.
    let _ = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{}:F", whoami()))
        .output();
}

#[cfg(windows)]
fn whoami() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "%USERNAME%".into())
}

#[cfg(not(windows))]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// An unlocked signing key.
///
/// Deliberately not `Clone`, not `Debug`, not serialisable, and there is no method that
/// returns the secret bytes. Signing happens here or not at all.
pub struct Wallet {
    keypair: Keypair,
}

impl Wallet {
    #[must_use]
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.keypair.sign_message(message)
    }

    /// Exposed for transaction signing, which needs the `Signer` itself.
    #[must_use]
    pub fn signer(&self) -> &Keypair {
        &self.keypair
    }
}

/// Raw key material on its way into [`EncryptedKey::seal`].
///
/// A newtype rather than `Zeroizing<Vec<u8>>` for one reason: `Zeroizing` is `Debug` if
/// its contents are, so a plain byte vector can be printed by anything that formats it.
/// This cannot. It zeroizes on drop like the inner type does.
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Accept the shapes a key actually arrives in, and reject everything else.
///
/// Two formats are in circulation and users do not reliably know which they have:
/// `solana-keygen` writes a JSON array of 64 numbers, wallets like Phantom export
/// base58. Guessing wrong is not dangerous, just confusing, so try both and say what
/// was expected if neither fits.
///
/// # Errors
/// If the text is neither a 64-byte base58 string nor a 64-element JSON byte array.
pub fn parse_secret(input: &str) -> Result<SecretBytes> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("no key provided");
    }

    if trimmed.starts_with('[') {
        let nums: Vec<i64> = serde_json::from_str(trimmed)
            .context("looks like a JSON array but could not be parsed")?;
        if nums.len() != KEYPAIR_LEN {
            bail!("a keypair array has {KEYPAIR_LEN} numbers, this one has {}", nums.len());
        }
        let mut out = Zeroizing::new(Vec::with_capacity(KEYPAIR_LEN));
        for n in nums {
            let b = u8::try_from(n).map_err(|_| anyhow!("{n} is not a byte value"))?;
            out.push(b);
        }
        return Ok(SecretBytes(out));
    }

    let decoded = bs58::decode(trimmed)
        .into_vec()
        .context("not valid base58, and not a JSON array either")?;
    if decoded.len() != KEYPAIR_LEN {
        bail!(
            "decoded to {} bytes; a full keypair is {KEYPAIR_LEN}. A {}-byte value is a \
             public key or a seed, neither of which can sign.",
            decoded.len(),
            decoded.len()
        );
    }
    Ok(SecretBytes(Zeroizing::new(decoded)))
}

/// Hex in the JSON rather than base64: it survives being looked at in a text editor,
/// and nothing here is large enough for the size difference to matter.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(v.len() * 2);
        for b in v {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("odd-length hex"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_keypair() -> SecretBytes {
        SecretBytes(Zeroizing::new(Keypair::new().to_bytes().to_vec()))
    }

    #[test]
    fn a_sealed_key_comes_back_as_the_same_address() {
        let secret = a_keypair();
        let expected = Keypair::try_from(secret.as_slice()).unwrap().pubkey();

        let sealed = EncryptedKey::seal(secret.as_slice(), "correct horse battery staple").unwrap();
        let w = sealed.unseal("correct horse battery staple").unwrap();

        assert_eq!(w.pubkey(), expected);
    }

    #[test]
    fn the_wrong_passphrase_is_refused_rather_than_returning_a_different_key() {
        let sealed = EncryptedKey::seal(a_keypair().as_slice(), "right").unwrap();
        assert!(sealed.unseal("wrong").is_err());
    }

    /// The whole point of encrypting it. If the secret appears in the file, everything
    /// else here is decoration.
    #[test]
    fn the_secret_never_appears_in_the_stored_form() {
        let secret = a_keypair();
        let sealed = EncryptedKey::seal(secret.as_slice(), "pass").unwrap();
        let json = serde_json::to_string(&sealed).unwrap();

        let as_b58 = bs58::encode(secret.as_slice()).into_string();
        assert!(!json.contains(&as_b58), "the key is sitting in the file in base58");

        // And no run of the raw bytes either, however they might be encoded.
        let mut hex = String::new();
        for b in secret.as_slice() {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        assert!(!json.contains(&hex), "the key is sitting in the file in hex");
    }

    /// A file someone has hand-edited to point at a different address must not sign.
    ///
    /// Matched rather than `unwrap_err`-ed because `Wallet` has no `Debug` — which is
    /// itself the point, and the compiler enforces it here.
    #[test]
    fn a_tampered_address_field_is_caught() {
        let mut sealed = EncryptedKey::seal(a_keypair().as_slice(), "pass").unwrap();
        sealed.pubkey = Keypair::new().pubkey().to_string();
        match sealed.unseal("pass") {
            Ok(_) => panic!("a file whose address was edited must not unlock"),
            Err(e) => assert!(e.to_string().contains("inconsistent"), "got: {e}"),
        }
    }

    #[test]
    fn ciphertext_tampering_is_caught_by_the_aead() {
        let mut sealed = EncryptedKey::seal(a_keypair().as_slice(), "pass").unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert!(sealed.unseal("pass").is_err());
    }

    #[test]
    fn an_empty_passphrase_is_refused_outright() {
        assert!(EncryptedKey::seal(a_keypair().as_slice(), "").is_err());
    }

    #[test]
    fn both_key_formats_people_actually_have_are_accepted() {
        let kp = Keypair::new();
        let bytes = kp.to_bytes();

        let from_b58 = parse_secret(&bs58::encode(bytes).into_string()).unwrap();
        assert_eq!(from_b58.as_slice(), bytes.as_slice());

        let arr = serde_json::to_string(&bytes.to_vec()).unwrap();
        let from_json = parse_secret(&arr).unwrap();
        assert_eq!(from_json.as_slice(), bytes.as_slice());
    }

    /// A 32-byte value is the mistake people actually make — pasting a public key, or a
    /// seed, and wondering why nothing signs.
    #[test]
    fn a_public_key_sized_input_is_rejected_with_an_explanation() {
        let pk = Keypair::new().pubkey().to_bytes();
        let err = match parse_secret(&bs58::encode(pk).into_string()) {
            Ok(_) => panic!("a 32-byte public key must not be accepted as a signing key"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("32"), "got: {err}");
        assert!(err.contains("public key"), "got: {err}");
    }

    #[test]
    fn rubbish_is_refused_without_panicking() {
        assert!(parse_secret("").is_err());
        assert!(parse_secret("not a key").is_err());
        assert!(parse_secret("[1,2,3]").is_err());
        assert!(parse_secret("[]").is_err());
    }

    #[test]
    fn a_sealed_key_survives_a_round_trip_through_a_file() {
        let dir = std::env::temp_dir().join("cb-wallet-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("key.json");

        let secret = a_keypair();
        let expected = Keypair::try_from(secret.as_slice()).unwrap().pubkey();
        EncryptedKey::seal(secret.as_slice(), "pass").unwrap().save(&path).unwrap();

        let loaded = EncryptedKey::load(&path).unwrap();
        assert_eq!(loaded.unseal("pass").unwrap().pubkey(), expected);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_signature_it_produces_verifies_against_its_own_address() {
        let sealed = EncryptedKey::seal(a_keypair().as_slice(), "pass").unwrap();
        let w = sealed.unseal("pass").unwrap();
        let msg = b"a transaction would go here";
        assert!(w.sign(msg).verify(&w.pubkey().to_bytes(), msg));
    }
}
