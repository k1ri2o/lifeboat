//! Forgiving input for the recovery key.

/// The characters a recovery key is made of (Crockford base32).
pub const KEY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Turn what a person typed into the exact recovery key, if it looks like one.
///
/// Upper-cases, drops spaces and dashes, reads O as 0 and I or L as 1, and when exactly 30
/// characters of the key alphabet remain, regroups them as `XXXXX-XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`.
/// Returns `None` when the input cannot be a recovery key.
pub fn normalize_recovery_key(input: &str) -> Option<String> {
    let mut chars = Vec::with_capacity(30);
    for c in input.chars() {
        if c.is_whitespace() || c == '-' {
            continue;
        }
        let c = match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            u => u,
        };
        if !c.is_ascii() || !KEY_ALPHABET.contains(&(c as u8)) {
            return None;
        }
        chars.push(c);
    }
    if chars.len() != 30 {
        return None;
    }
    let groups: Vec<String> = chars.chunks(5).map(|g| g.iter().collect()).collect();
    Some(groups.join("-"))
}

/// Check `raw` with `verify`; when it fails and the normalised recovery key differs, try that.
///
/// Returns the string that opened the repository, or `None` when neither did. One call is one
/// guess for the lockout, however many strings are tried.
pub fn verify_forgiving(
    raw: &str,
    mut verify: impl FnMut(&str) -> Result<bool, String>,
) -> Result<Option<String>, String> {
    if verify(raw)? {
        return Ok(Some(raw.to_string()));
    }
    match normalize_recovery_key(raw) {
        Some(k) if k != raw => Ok(verify(&k)?.then_some(k)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ABCDE-FGHJK-MNPQR-STVWX-YZ012-34567";

    #[test]
    fn normalizes_spaces_case_and_lookalikes() {
        assert_eq!(normalize_recovery_key(KEY).as_deref(), Some(KEY));
        assert_eq!(
            normalize_recovery_key("abcde fghjk mnpqr stvwx yz012 34567").as_deref(),
            Some(KEY)
        );
        assert_eq!(
            normalize_recovery_key(" ABCDEFGHJKMNPQRSTVWXYZ01234567 ").as_deref(),
            Some(KEY)
        );
        assert_eq!(
            normalize_recovery_key("ABCDE-FGHJK-MNPQR-STVWX-YZOIL-34567").as_deref(),
            Some("ABCDE-FGHJK-MNPQR-STVWX-YZ011-34567")
        );
    }

    #[test]
    fn rejects_things_that_are_not_keys() {
        assert_eq!(normalize_recovery_key(""), None);
        assert_eq!(normalize_recovery_key("my long password here"), None);
        assert_eq!(normalize_recovery_key("ABCDE-FGHJK"), None);
        assert_eq!(normalize_recovery_key(&format!("{KEY}A")), None);
        assert_eq!(
            normalize_recovery_key("ABCDE-FGHJK-MNPQR-STVWX-YZ012-3456U"),
            None
        );
        assert_eq!(
            normalize_recovery_key("ÄBCDE-FGHJK-MNPQR-STVWX-YZ012-34567"),
            None
        );
    }

    #[test]
    fn retry_uses_the_normalised_key_only_when_raw_fails() {
        let mut tried = Vec::new();
        let got = verify_forgiving("abcde fghjk mnpqr stvwx yz012 34567", |p| {
            tried.push(p.to_string());
            Ok(p == KEY)
        });
        assert_eq!(got, Ok(Some(KEY.to_string())));
        assert_eq!(tried.len(), 2);

        // Raw works: no second try.
        let mut n = 0;
        let got = verify_forgiving("pw", |p| {
            n += 1;
            Ok(p == "pw")
        });
        assert_eq!(got, Ok(Some("pw".into())));
        assert_eq!(n, 1);

        // Neither works, or input is not key-shaped: one or two tries, then None.
        assert_eq!(verify_forgiving("nope", |_| Ok(false)), Ok(None));
        assert_eq!(verify_forgiving(KEY, |_| Ok(false)), Ok(None));
        // Errors pass through.
        assert!(verify_forgiving("x", |_| Err("boom".into())).is_err());
    }
}
