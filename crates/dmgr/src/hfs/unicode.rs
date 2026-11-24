//! HFS+ file name handling.
//!
//! HFS+ stores names as UTF-16 in (Apple's variant of) Unicode canonical
//! decomposition, and the POSIX layer swaps `:` and `/`. Case-insensitive
//! volumes order catalog keys with `FastUnicodeCompare`, case-sensitive
//! (HFSX) volumes with a plain binary comparison of UTF-16 code units.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

use super::case_table::{LATIN_CASE_FOLD, LOWER_CASE_TABLE};
use crate::error::{Result, invalid};

/// Maximum length of an HFS+ name in UTF-16 code units.
pub const MAX_NAME_LEN: usize = 255;

/// Ranges Apple's decomposition leaves untouched (TN1150, "Unicode Subtleties").
fn decomposition_excluded(c: char) -> bool {
    matches!(c as u32, 0x2000..=0x2FFF | 0xF900..=0xFAFF | 0x2F800..=0x2FAFF)
}

/// Decomposes a name the way HFS+ stores it, without the `:`/`/` swap.
pub fn decompose(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut run = String::new();
    for c in name.chars() {
        if decomposition_excluded(c) {
            out.extend(run.nfd());
            run.clear();
            out.push(c);
        } else {
            run.push(c);
        }
    }
    out.extend(run.nfd());
    out
}

/// Converts a POSIX file name into the UTF-16 form stored in the catalog.
pub fn to_hfs_name(name: &str) -> Result<Vec<u16>> {
    if name.is_empty() {
        return Err(invalid("empty file name"));
    }
    if name.contains('\0') {
        return Err(invalid(format!("file name {name:?} contains NUL")));
    }
    let units: Vec<u16> = decompose(name)
        .encode_utf16()
        .map(|u| match u {
            0x3A => 0x2F, // ':' -> '/'
            0x2F => 0x3A, // '/' -> ':' (only reachable for volume names)
            u => u,
        })
        .collect();
    if units.len() > MAX_NAME_LEN {
        return Err(invalid(format!(
            "file name {name:?} is longer than {MAX_NAME_LEN} UTF-16 units after decomposition"
        )));
    }
    Ok(units)
}

/// Converts a catalog name back to a POSIX file name.
pub fn from_hfs_name(units: &[u16]) -> String {
    let swapped: Vec<u16> = units
        .iter()
        .map(|&u| match u {
            0x2F => 0x3A,
            0x3A => 0x2F,
            u => u,
        })
        .collect();
    String::from_utf16_lossy(&swapped)
}

#[inline]
fn fold(c: u16) -> u16 {
    if c < 0x100 {
        return LATIN_CASE_FOLD[c as usize];
    }
    let idx = LOWER_CASE_TABLE[(c >> 8) as usize];
    if idx == 0 {
        c
    } else {
        LOWER_CASE_TABLE[idx as usize + (c & 0xFF) as usize]
    }
}

/// Iterator over the case-folded, non-ignorable code units of a name.
fn folded(s: &[u16]) -> impl Iterator<Item = u16> + '_ {
    s.iter().map(|&c| fold(c)).filter(|&c| c != 0)
}

/// Apple's `FastUnicodeCompare`: case-insensitive ordering used by HFS+.
pub fn fast_unicode_compare(a: &[u16], b: &[u16]) -> Ordering {
    folded(a).cmp(folded(b))
}

/// Binary ordering used by case-sensitive HFSX volumes.
pub fn binary_compare(a: &[u16], b: &[u16]) -> Ordering {
    a.cmp(b)
}

/// The key that two names share exactly when they compare equal under
/// `FastUnicodeCompare`.
pub fn fold_key(s: &[u16]) -> Vec<u16> {
    folded(s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(fast_unicode_compare(&u("ABC"), &u("abc")), Ordering::Equal);
        assert_eq!(fast_unicode_compare(&u("abc"), &u("abd")), Ordering::Less);
        assert_eq!(fast_unicode_compare(&u("ab"), &u("abc")), Ordering::Less);
        assert_eq!(fast_unicode_compare(&u(""), &u("a")), Ordering::Less);
        // Greek capital sigma vs small sigma
        assert_eq!(
            fast_unicode_compare(&u("\u{03A3}"), &u("\u{03C3}")),
            Ordering::Equal
        );
        // NUL sorts after everything (maps to 0xFFFF)
        assert_eq!(fast_unicode_compare(&[0], &u("z")), Ordering::Greater);
        // Ignorable characters (ZERO WIDTH JOINER) are skipped
        assert_eq!(
            fast_unicode_compare(&u("a\u{200D}b"), &u("ab")),
            Ordering::Equal
        );
        // underscore (0x5F) sorts after letters once they fold to lowercase? No:
        // 'B' folds to 'b' (0x62) which is > '_' (0x5F).
        assert_eq!(fast_unicode_compare(&u("_"), &u("B")), Ordering::Less);
        assert_eq!(binary_compare(&u("_"), &u("B")), Ordering::Greater);
    }

    #[test]
    fn names() {
        assert_eq!(to_hfs_name("a:b").unwrap(), u("a/b"));
        assert_eq!(from_hfs_name(&u("a/b")), "a:b");
        // é decomposes to e + combining acute
        assert_eq!(to_hfs_name("\u{e9}").unwrap(), vec![0x65, 0x301]);
        // CJK compatibility ideographs are left alone
        assert_eq!(to_hfs_name("\u{F900}").unwrap(), vec![0xF900]);
        // Hangul syllables are decomposed
        assert_eq!(to_hfs_name("\u{AC00}").unwrap(), vec![0x1100, 0x1161]);
        assert!(to_hfs_name("").is_err());
        assert!(to_hfs_name(&"x".repeat(256)).is_err());
        assert!(to_hfs_name(&"x".repeat(255)).is_ok());
    }
}
