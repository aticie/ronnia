//! AES-256-GCM envelopes for stored Twitch credentials.
//!
//! Tokens are serialized, encrypted with the configuration key, and stored as a
//! nonce-prefixed ciphertext bound to the user id as associated data. Nothing in this
//! module formats token material: `Credentials` derives neither `Debug` nor `Clone`.

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::AeadInPlace};
use anyhow::{Result, anyhow, ensure};
use rand::RngCore;
use serde::{Deserialize, Serialize};

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const AAD_VERSION: &[u8] = b"ronnia:twitch-bot-auth:v1\0";

// Deliberately neither Debug nor Clone: token material must never reach diagnostics.
#[derive(Deserialize, Serialize)]
pub(super) struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
}

fn associated_data(user_id: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_VERSION.len() + user_id.len());
    aad.extend_from_slice(AAD_VERSION);
    aad.extend_from_slice(user_id.as_bytes());
    aad
}

pub(super) fn encrypt(key: &[u8; 32], user_id: &str, tokens: &Credentials) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow!("invalid Twitch token encryption key"))?;
    let mut nonce = [0_u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);
    let mut body =
        serde_json::to_vec(tokens).map_err(|_| anyhow!("encoding Twitch credentials failed"))?;
    cipher
        .encrypt_in_place(
            Nonce::from_slice(&nonce),
            &associated_data(user_id),
            &mut body,
        )
        .map_err(|_| anyhow!("encrypting Twitch credentials failed"))?;
    // Reuse the ciphertext allocation for the nonce-prefixed storage envelope.
    body.extend_from_slice(&nonce);
    body.rotate_right(NONCE_LEN);
    Ok(body)
}

pub(super) fn decrypt(key: &[u8; 32], user_id: &str, ciphertext: &[u8]) -> Result<Credentials> {
    ensure!(
        ciphertext.len() >= NONCE_LEN + TAG_LEN,
        "invalid encrypted Twitch credentials"
    );
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow!("invalid Twitch token encryption key"))?;
    let mut body = ciphertext[NONCE_LEN..].to_vec();
    cipher
        .decrypt_in_place(
            Nonce::from_slice(&ciphertext[..NONCE_LEN]),
            &associated_data(user_id),
            &mut body,
        )
        .map_err(|_| anyhow!("cannot decrypt Twitch credentials; check TWITCH_TOKEN_KEY"))?;
    serde_json::from_slice(&body).map_err(|_| anyhow!("invalid encrypted Twitch credentials"))
}

#[cfg(test)]
mod tests {
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
}
