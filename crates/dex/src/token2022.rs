//! What a Token-2022 mint takes from a transfer, read from its extensions.
//!
//! A pool account says nothing about its mints' extensions, and two of them change what
//! a swap actually delivers: a **transfer fee** skims every movement of the token, and a
//! **transfer hook** runs another program on every movement, which needs accounts no
//! swap here passes. A quote that ignores either overstates what arrives, so a pool
//! holding such a mint is refused by the caller that has the mint account.
//!
//! Layout (SPL Token-2022): the 82-byte base mint, padding to byte 165, an account-type
//! byte (1 = mint) at 165, then type-length-value extensions from byte 166: a `u16`
//! type, a `u16` length, the value. `TransferFeeConfig` is type 1; its value ends with
//! the older and newer fee (`epoch u64, maximum_fee u64, basis_points u16` each).
//! `TransferHook` is type 14; its value is an authority and a program id, zero when
//! unset.
//!
//! Two more stop a swap outright rather than skimming it, and are screened for the same
//! reason — a pool that can only revert still shows a price, and chasing it wastes every
//! attempt: `DefaultAccountState` (type 6) set to frozen makes the token account a swap
//! opens unusable, and `Pausable` (type 26; an authority, then a paused flag) with the
//! flag set refuses every transfer. Tokenised equities carry both extensions, unset.

use anyhow::{ensure, Result};

const BASE_MINT_LEN: usize = 82;
const ACCOUNT_TYPE_OFFSET: usize = 165;
const TLV_START: usize = 166;
const TRANSFER_FEE_CONFIG: u16 = 1;
const DEFAULT_ACCOUNT_STATE: u16 = 6;
const TRANSFER_HOOK: u16 = 14;
const PAUSABLE: u16 = 26;
/// `AccountState::Frozen` in a `DefaultAccountState` extension.
const FROZEN: u8 = 2;

/// A mint's transfer costs: the larger of its two scheduled fees in basis points, and
/// whether a transfer hook program is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferCosts {
    pub fee_bps: u16,
    pub hook: bool,
    /// New token accounts start frozen, so the one a swap opens cannot receive.
    pub frozen_by_default: bool,
    /// Transfers are paused by the mint's authority.
    pub paused: bool,
}

impl TransferCosts {
    /// Whether a transfer of this mint, into an account a swap opens, delivers exactly
    /// what was sent.
    #[must_use]
    pub fn is_free(&self) -> bool {
        self.fee_bps == 0 && !self.hook && !self.frozen_by_default && !self.paused
    }
}

/// Read a mint account's transfer costs. A classic mint (no extensions) costs nothing.
///
/// # Errors
/// If the account is shorter than a mint, or an extension runs past its end.
pub fn transfer_costs(data: &[u8]) -> Result<TransferCosts> {
    ensure!(data.len() >= BASE_MINT_LEN, "a mint is at least {BASE_MINT_LEN} bytes, got {}", data.len());
    let mut out = TransferCosts::default();
    if data.len() <= TLV_START || data[ACCOUNT_TYPE_OFFSET] != 1 {
        return Ok(out);
    }
    let mut o = TLV_START;
    while o + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[o], data[o + 1]]);
        let len = usize::from(u16::from_le_bytes([data[o + 2], data[o + 3]]));
        o += 4;
        if kind == 0 {
            break; // Uninitialized: the rest is padding.
        }
        ensure!(o + len <= data.len(), "extension {kind} runs past the end of the mint");
        let value = &data[o..o + len];
        match kind {
            TRANSFER_FEE_CONFIG if len >= 108 => {
                // older fee at 72..90, newer at 90..108; basis points are each one's last u16.
                let older = u16::from_le_bytes([value[88], value[89]]);
                let newer = u16::from_le_bytes([value[106], value[107]]);
                out.fee_bps = older.max(newer);
            }
            TRANSFER_HOOK if len >= 64 => out.hook = value[32..64].iter().any(|&b| b != 0),
            DEFAULT_ACCOUNT_STATE if len >= 1 => out.frozen_by_default = value[0] == FROZEN,
            PAUSABLE if len >= 33 => out.paused = value[32] != 0,
            _ => {}
        }
        o += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mint_with(extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut d = vec![0u8; TLV_START];
        d[ACCOUNT_TYPE_OFFSET] = 1;
        for (kind, value) in extensions {
            d.extend_from_slice(&kind.to_le_bytes());
            d.extend_from_slice(&(value.len() as u16).to_le_bytes());
            d.extend_from_slice(value);
        }
        d
    }

    #[test]
    fn a_classic_mint_costs_nothing() {
        assert!(transfer_costs(&[0u8; BASE_MINT_LEN]).unwrap().is_free());
    }

    #[test]
    fn a_transfer_fee_is_read_from_either_schedule() {
        let mut v = vec![0u8; 108];
        v[106..108].copy_from_slice(&50u16.to_le_bytes());
        let c = transfer_costs(&mint_with(&[(TRANSFER_FEE_CONFIG, v.clone())])).unwrap();
        assert_eq!(c.fee_bps, 50);
        assert!(!c.is_free());
        let mut zero = vec![0u8; 108];
        zero[0] = 9; // an authority, but no fee
        assert!(transfer_costs(&mint_with(&[(TRANSFER_FEE_CONFIG, zero)])).unwrap().is_free());
    }

    #[test]
    fn a_hook_counts_only_when_its_program_is_set() {
        let mut set = vec![0u8; 64];
        set[40] = 1;
        assert!(transfer_costs(&mint_with(&[(TRANSFER_HOOK, set)])).unwrap().hook);
        let unset = vec![7u8; 32].into_iter().chain(vec![0u8; 32]).collect();
        assert!(!transfer_costs(&mint_with(&[(TRANSFER_HOOK, unset)])).unwrap().hook);
    }

    #[test]
    fn a_frozen_default_or_a_pause_blocks_and_their_unset_forms_do_not() {
        assert!(transfer_costs(&mint_with(&[(DEFAULT_ACCOUNT_STATE, vec![FROZEN])])).unwrap().frozen_by_default);
        assert!(transfer_costs(&mint_with(&[(DEFAULT_ACCOUNT_STATE, vec![1])])).unwrap().is_free());
        let mut paused = vec![3u8; 32];
        paused.push(1);
        assert!(transfer_costs(&mint_with(&[(PAUSABLE, paused)])).unwrap().paused);
        let mut running = vec![3u8; 32];
        running.push(0);
        let c = transfer_costs(&mint_with(&[(PAUSABLE, running), (DEFAULT_ACCOUNT_STATE, vec![1])])).unwrap();
        assert!(c.is_free(), "a tokenised equity as it trades: pausable, not paused");
    }

    #[test]
    fn other_extensions_are_walked_past() {
        let c = transfer_costs(&mint_with(&[(18, vec![1u8; 40]), (TRANSFER_HOOK, vec![0u8; 64])])).unwrap();
        assert!(c.is_free());
    }
}
