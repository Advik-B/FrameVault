//! Memory budget: turns an explicit `--memory` size or a fraction of total system RAM
//! into a count of RS blocks, used to size encode's streaming batch and decode's sliding
//! window. Purely local and per-invocation — it has no wire-format impact, since each
//! batch/window is Reed-Solomon decoded independently (see `rs.rs`).

use anyhow::{bail, Result};
use sysinfo::System;

use crate::constants::RS_BLOCK_SIZE;

/// Fraction of total system RAM used as the default budget when `--memory` isn't given.
const DEFAULT_RAM_FRACTION: f64 = 0.25;

/// Floor applied only to the RAM-derived default, guarding against pathologically
/// small results (e.g. RAM detection failing or returning near-zero). An explicit
/// `--memory` value is never clamped against this — see [`blocks_for_budget`].
const MIN_DEFAULT_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Parse a human-supplied size string ("512M", "2G", "1024K", or a bare byte count,
/// optionally fractional) into a byte count. Rejects zero, negative, and non-numeric input.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty size");
    }
    let (num, mult) = match s[..s.len() - 1].len() {
        _ if s.ends_with(['k', 'K']) => (&s[..s.len() - 1], 1024u64),
        _ if s.ends_with(['m', 'M']) => (&s[..s.len() - 1], 1024 * 1024),
        _ if s.ends_with(['g', 'G']) => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid memory size: {s:?}"))?;
    if !n.is_finite() || n <= 0.0 {
        bail!("memory size must be a positive number: {s:?}");
    }
    Ok((n * mult as f64) as u64)
}

/// Resolve the effective memory budget in bytes: the explicit value if given (parsed
/// as-is, no floor), else `DEFAULT_RAM_FRACTION` of total system RAM, floored at
/// `MIN_DEFAULT_BUDGET_BYTES`.
pub fn resolve_budget_bytes(explicit: Option<&str>) -> Result<u64> {
    match explicit {
        Some(s) => parse_size(s),
        None => {
            let mut sys = System::new();
            sys.refresh_memory();
            let total = sys.total_memory(); // bytes (sysinfo 0.3x)
            let derived = (total as f64 * DEFAULT_RAM_FRACTION) as u64;
            Ok(derived.max(MIN_DEFAULT_BUDGET_BYTES))
        }
    }
}

/// Convert a byte budget into a whole number of RS blocks (each `RS_BLOCK_SIZE` bytes
/// of encoded ECC). Always at least 1, so an absurdly small budget still makes progress.
pub fn blocks_for_budget(budget_bytes: u64) -> usize {
    ((budget_bytes / RS_BLOCK_SIZE as u64) as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_suffixes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1.5G").unwrap(), (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size("  64M  ").unwrap(), 64 * 1024 * 1024);
    }

    #[test]
    fn parse_size_rejects_invalid() {
        assert!(parse_size("0").is_err());
        assert!(parse_size("-5M").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("not a size").is_err());
        assert!(parse_size("M").is_err());
    }

    #[test]
    fn blocks_for_budget_minimum_one() {
        assert_eq!(blocks_for_budget(0), 1);
        assert_eq!(blocks_for_budget(1), 1);
        assert_eq!(blocks_for_budget(RS_BLOCK_SIZE as u64 - 1), 1);
        assert_eq!(blocks_for_budget(RS_BLOCK_SIZE as u64), 1);
        assert_eq!(blocks_for_budget(RS_BLOCK_SIZE as u64 * 4), 4);
    }

    #[test]
    fn explicit_budget_not_floored() {
        // An explicit tiny budget is respected (down to blocks_for_budget's own
        // minimum-1 guarantee), unlike the RAM-derived default.
        let bytes = resolve_budget_bytes(Some("1K")).unwrap();
        assert_eq!(bytes, 1024);
        assert!(blocks_for_budget(bytes) >= 1);
    }

    #[test]
    fn default_budget_has_a_floor() {
        let bytes = resolve_budget_bytes(None).unwrap();
        assert!(bytes >= MIN_DEFAULT_BUDGET_BYTES);
    }
}
