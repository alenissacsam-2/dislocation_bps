//! Instruction encoding primitives, and the program addresses everything else names.
//!
//! # Why discriminators are computed rather than pasted
//!
//! An Anchor instruction is identified by the first eight bytes of
//! `sha256("global:<method_name>")`. Every published integration guide gives these as
//! hex constants, and copying one is a silent single point of failure: a transposed
//! nibble produces a discriminator that matches no method, and the program's error is
//! `InstructionFallbackNotFound` — which reads like a wrong *account*, not a wrong
//! *byte*, and sends you looking in the wrong place.
//!
//! So the name is the input and the hash is derived. `swap` being spelled `swap` is
//! something a reader can check; `f8c69e91e17587c8` is not.

use solana_sdk::hash::hashv;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// The first eight bytes of `sha256("global:<name>")`.
///
/// # Panics
/// Never — the hash is always at least eight bytes.
#[must_use]
pub fn anchor_discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("global:{name}");
    let h = hashv(&[preimage.as_bytes()]);
    let mut out = [0u8; 8];
    out.copy_from_slice(&h.to_bytes()[..8]);
    out
}

/// A little-endian argument buffer, built discriminator-first.
///
/// Borsh, for the primitive types an instruction argument list actually uses, is just
/// little-endian with `bool` as one byte. Pulling in a derive macro to express that
/// would be more code than the six methods below.
#[derive(Debug, Clone)]
pub struct Args {
    buf: Vec<u8>,
}

impl Args {
    /// Start an argument buffer for an Anchor method.
    #[must_use]
    pub fn anchor(method: &str) -> Self {
        Self { buf: anchor_discriminator(method).to_vec() }
    }

    /// Start an argument buffer for a program that tags instructions with a single
    /// byte rather than an Anchor discriminator — the SPL programs do this.
    #[must_use]
    pub fn tagged(tag: u8) -> Self {
        Self { buf: vec![tag] }
    }

    #[must_use]
    pub fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }

    #[must_use]
    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    pub fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    pub fn u128(mut self, v: u128) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    pub fn bool(self, v: bool) -> Self {
        self.u8(u8::from(v))
    }

    #[must_use]
    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Parse a program address that is a compile-time constant in this codebase.
///
/// # Panics
/// If the string is not valid base58 of the right length. Every caller passes a
/// literal that is covered by a test, so a panic here is a build-time error that
/// happens to fire at run time.
#[must_use]
pub fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("hard-coded program address must parse")
}

/// Convert this codebase's raw 32-byte key into the SDK's.
#[must_use]
pub fn to_pubkey(raw: &cb_core::types::Pubkey32) -> Pubkey {
    Pubkey::new_from_array(*raw)
}

/// Convert back, for handing an address to the pure crates.
#[must_use]
pub fn to_raw(k: &Pubkey) -> cb_core::types::Pubkey32 {
    k.to_bytes()
}

/// Program addresses. All of these are load-bearing and all are checked by a test.
pub mod programs {
    pub const SYSTEM: &str = "11111111111111111111111111111111";
    pub const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    pub const SPL_TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
    pub const ASSOCIATED_TOKEN: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    pub const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
    /// Wrapped SOL. A mint, not a program, but it belongs with the other constants
    /// that are true of mainnet rather than of a pool.
    pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// If `hashv` were ever not sha256, every discriminator in this crate would be
    /// wrong together and no other test would notice. This is the anchor for all of
    /// them: the sha256 of the empty string is a published constant.
    #[test]
    fn hashv_is_sha256() {
        let h = hashv(&[b""]);
        assert_eq!(
            h.to_string(),
            // sha256("") in base58, which is how solana renders a Hash.
            {
                let bytes: [u8; 32] = [
                    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99,
                    0x6f, 0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95,
                    0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
                ];
                bs58::encode(bytes).into_string()
            }
        );
    }

    #[test]
    fn a_discriminator_is_eight_bytes_of_the_namespaced_hash() {
        let d = anchor_discriminator("swap");
        let full = hashv(&[b"global:swap"]);
        assert_eq!(d, full.to_bytes()[..8]);
    }

    /// Different methods must not collide, or one venue's swap would invoke another's
    /// method with arguments shaped for the first.
    #[test]
    fn distinct_methods_have_distinct_discriminators() {
        let names = ["swap", "swapV2", "swap_v2", "two_hop_swap", "swap_base_input"];
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(
                    anchor_discriminator(a),
                    anchor_discriminator(b),
                    "{a} and {b} collide"
                );
            }
        }
    }

    #[test]
    fn args_pack_little_endian_after_the_discriminator() {
        let d = anchor_discriminator("swap");
        let out = Args::anchor("swap").u64(1).u64(0).u128(2).bool(true).bool(false).build();
        assert_eq!(&out[..8], &d);
        assert_eq!(&out[8..16], &1u64.to_le_bytes());
        assert_eq!(&out[16..24], &0u64.to_le_bytes());
        assert_eq!(&out[24..40], &2u128.to_le_bytes());
        assert_eq!(out[40], 1);
        assert_eq!(out[41], 0);
        assert_eq!(out.len(), 42);
    }

    #[test]
    fn a_tagged_instruction_starts_with_one_byte_not_eight() {
        assert_eq!(Args::tagged(17).build(), vec![17]);
        assert_eq!(Args::tagged(1).u64(5).build().len(), 9);
    }

    /// A mistyped program address is the kind of error that produces a transaction
    /// which fails for a reason that has nothing to do with the mistake.
    #[test]
    fn every_program_address_parses_and_is_distinct() {
        use programs::*;
        let all = [
            SYSTEM,
            SPL_TOKEN,
            SPL_TOKEN_2022,
            ASSOCIATED_TOKEN,
            COMPUTE_BUDGET,
            WSOL_MINT,
        ];
        let parsed: Vec<Pubkey> = all.iter().map(|s| pk(s)).collect();
        for (i, a) in parsed.iter().enumerate() {
            assert_eq!(a.to_string(), all[i], "{} does not round trip", all[i]);
            for b in &parsed[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn raw_and_sdk_keys_round_trip() {
        let raw: cb_core::types::Pubkey32 = [7u8; 32];
        assert_eq!(to_raw(&to_pubkey(&raw)), raw);
    }
}
