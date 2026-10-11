//! The repository proof file: the shared format between the
//! publisher's `orchard.txt` (written by `orchard-publish --prove`)
//! and the client's verification. Hex pubkeys, one per line, `#`
//! comments and blanks ignored, an optional `0x` tolerated.

/// Parse the proof file into its hex keys. None (no keys) on an
/// empty or unparsable file: a proof that names nothing verifies
/// nothing.
pub fn parse_proof_keys(body: &str) -> Option<Vec<String>> {
    let keys: Vec<String> = body
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.trim_start_matches("0x").to_ascii_lowercase())
        .filter(|line| line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit()))
        .collect();
    (!keys.is_empty()).then_some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_parsing_takes_hex_lines_and_comments() {
        let key_a = "ab12".repeat(16);
        let key_b = "cd34".to_string() + &"ef56".repeat(15);
        let body = format!("# header\n{key_a}\n\n0x{key_b}\ntalk\n");
        assert_eq!(parse_proof_keys(&body), Some(vec![key_a, key_b]));
        assert_eq!(parse_proof_keys("# nothing but a comment\n"), None);
    }
}
