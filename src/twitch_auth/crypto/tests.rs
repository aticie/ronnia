use super::*;

#[test]
fn encrypted_envelopes_are_random_and_bound_to_key_and_identity() {
    let tokens = Credentials {
        access_token: "secret-access-token".into(),
        refresh_token: "secret-refresh-token".into(),
    };
    let first = encrypt(&[7; 32], "123", &tokens).unwrap();
    let second = encrypt(&[7; 32], "123", &tokens).unwrap();
    assert_ne!(first, second);
    assert!(
        !first
            .windows(tokens.access_token.len())
            .any(|w| w == tokens.access_token.as_bytes())
    );
    assert!(
        !first
            .windows(tokens.refresh_token.len())
            .any(|w| w == tokens.refresh_token.as_bytes())
    );
    let decoded = decrypt(&[7; 32], "123", &first).unwrap();
    assert_eq!(decoded.access_token, tokens.access_token);
    assert_eq!(decoded.refresh_token, tokens.refresh_token);
    assert!(decrypt(&[8; 32], "123", &first).is_err());
    assert!(decrypt(&[7; 32], "456", &first).is_err());
    let mut tampered = first;
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    assert!(decrypt(&[7; 32], "123", &tampered).is_err());
    assert!(decrypt(&[7; 32], "123", &[0; 12]).is_err());
}
