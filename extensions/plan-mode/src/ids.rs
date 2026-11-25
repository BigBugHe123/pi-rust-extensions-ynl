//! Random version 4 UUIDs from the randomly keyed std hasher, without a crate.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

fn random_u64(salt: u64) -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.write_u128(nanos);
    hasher.write_u64(salt);
    hasher.write_u32(std::process::id());
    hasher.finish()
}

pub fn uuid() -> String {
    let high = random_u64(1);
    let low = random_u64(2);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// A lowercase version 4 UUID, as upstream `randomUUID` writes.
pub fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    let hex = |s: &str| s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, part)| part.len() == *len && hex(part))
        && parts[2].starts_with('4')
        && parts[3].starts_with(['8', '9', 'a', 'b'])
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn uuids_are_v4_and_differ() {
        let (a, b) = (super::uuid(), super::uuid());
        assert!(super::is_uuid(&a) && super::is_uuid(&b), "{a} {b}");
        assert_ne!(a, b);
        assert!(!super::is_uuid("not-a-uuid"));
    }
}
