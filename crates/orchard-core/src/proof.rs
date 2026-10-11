//! The repository proof file: the shared format between the
//! publisher's `orchard.txt` (written by `orchard-publish --prove`)
//! and the client's verification. One key per line, hex or npub
//! (bech32, checksummed, what the UI shows), `#` comments and blanks
//! ignored, an optional `0x` tolerated on hex.

/// A parsed key line: hex as the events carry it, or an npub the
/// consumer converts (this crate does not depend on the nostr
/// codec; both binaries that use proofs do).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofKey {
    Hex(String),
    Npub(String),
}

/// Parse the proof file into its key tokens. None (no keys) on an
/// empty or unparsable file: a proof that names nothing verifies
/// nothing.
pub fn parse_proof_keys(body: &str) -> Option<Vec<ProofKey>> {
    let keys: Vec<ProofKey> = body
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            if let Some(hex) = line.strip_prefix("0x") {
                is_hex(hex).then(|| ProofKey::Hex(hex.to_ascii_lowercase()))
            } else if is_hex(line) {
                Some(ProofKey::Hex(line.to_ascii_lowercase()))
            } else if line.starts_with("npub1")
                && line.len() >= 63
                && line[5..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                Some(ProofKey::Npub(line.to_string()))
            } else {
                None
            }
        })
        .collect();
    (!keys.is_empty()).then_some(keys)
}

fn is_hex(line: &str) -> bool {
    line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_parsing_takes_hex_npub_and_comments() {
        let key_a = "ab12".repeat(16);
        // an npub line passes through as its own kind: the consumer
        // converts through the nostr codec
        let npub = "npub1wqmqjss4wszcywfjhmgxvjq4jsm3cjsm5u9qk7zqzeuflqhv4cvsdpzmx2".to_string();
        let body = format!("# header\n{key_a}\n\n0xCD34{}\n{npub}\ntalk\n", "ef56".repeat(15));
        assert_eq!(
            parse_proof_keys(&body),
            Some(vec![
                ProofKey::Hex(key_a),
                ProofKey::Hex("cd34".to_string() + &"ef56".repeat(15)),
                ProofKey::Npub(npub),
            ])
        );
        assert_eq!(parse_proof_keys("# nothing but a comment\n"), None);
        // a short hex line is talk, not a key
        assert_eq!(parse_proof_keys("ab12\n"), None);
    }
}
