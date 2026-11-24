//! HFS+ timestamps: unsigned 32-bit seconds since 1904-01-01 00:00:00 UTC.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds between the HFS epoch (1904) and the Unix epoch (1970).
pub const HFS_EPOCH_OFFSET: u64 = 2_082_844_800;

/// Converts a [`SystemTime`] to an HFS+ timestamp, clamping to the
/// representable range (1904..=2040).
pub fn to_hfs(t: SystemTime) -> u32 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() + HFS_EPOCH_OFFSET).min(u64::from(u32::MAX)) as u32,
        Err(e) => HFS_EPOCH_OFFSET.saturating_sub(e.duration().as_secs()) as u32,
    }
}

/// Converts an HFS+ timestamp to a [`SystemTime`].
pub fn from_hfs(t: u32) -> SystemTime {
    let t = u64::from(t);
    if t >= HFS_EPOCH_OFFSET {
        UNIX_EPOCH + Duration::from_secs(t - HFS_EPOCH_OFFSET)
    } else {
        UNIX_EPOCH - Duration::from_secs(HFS_EPOCH_OFFSET - t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(to_hfs(UNIX_EPOCH), HFS_EPOCH_OFFSET as u32);
        assert_eq!(from_hfs(HFS_EPOCH_OFFSET as u32), UNIX_EPOCH);
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(from_hfs(to_hfs(t)), t);
        assert_eq!(to_hfs(UNIX_EPOCH - Duration::from_secs(10)), 2_082_844_790);
        assert_eq!(
            to_hfs(UNIX_EPOCH + Duration::from_secs(u64::from(u32::MAX))),
            u32::MAX
        );
    }
}
