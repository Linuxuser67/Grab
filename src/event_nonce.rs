//! Invocation-scoped identifiers for subprocess progress records.

/// Generate a random, delimiter-safe nonce for one child-process invocation.
pub(crate) fn new_event_nonce() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        nonce.push(char::from(HEX[usize::from(byte >> 4)]));
        nonce.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(nonce)
}

#[cfg(test)]
mod tests {
    use super::new_event_nonce;

    #[test]
    fn nonce_is_hex_and_unique_per_run() {
        let first = new_event_nonce().expect("operating-system randomness");
        let second = new_event_nonce().expect("operating-system randomness");
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
