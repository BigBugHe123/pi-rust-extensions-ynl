//! Context boundaries for contracts that the `context` hook inserts without a session message.
//! A boundary records the number of messages before the contract and a hash of those messages, so
//! later contexts put the contract back at the same place until compaction changes that prefix.

use serde_json::Value;

use crate::state::Boundary;

/// FNV-1a 64 of the JSON of `messages`, as 16 hex digits.
pub fn hash(messages: &[Value]) -> String {
    let json = serde_json::to_string(messages).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in json.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn is_hash(value: &str) -> bool {
    value.len() == 16 && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

pub fn boundary(messages: &[Value]) -> Boundary {
    Boundary {
        length: messages.len(),
        hash: hash(messages),
    }
}

pub fn matches(messages: &[Value], boundary: &Boundary) -> bool {
    boundary.length <= messages.len() && hash(&messages[..boundary.length]) == boundary.hash
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn boundary_matches_its_prefix_only() {
        let messages = vec![json!({ "role": "user", "content": "a" })];
        let b = boundary(&messages);
        let mut longer = messages.clone();
        longer.push(json!({ "role": "assistant", "content": [] }));
        assert!(matches(&longer, &b));
        assert!(!matches(&[json!({ "role": "user", "content": "b" })], &b));
        assert!(is_hash(&b.hash));
    }
}
