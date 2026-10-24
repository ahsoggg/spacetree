//! Release signatures. A SHA-256 published next to the exe on GitHub protects against a corrupt download, not
//! against someone who controls the release (stolen account or token): they replace the exe and its hash alike.
//! So the updater also requires an Ed25519 signature made with a key that never leaves the maintainer's machine;
//! the public half is compiled into the exe.
//!
//! The signed message binds the version and the SHA-256 of the exe, so a signature cannot be reused for another
//! file or for an older release published under a newer tag.
//!
//! This file is also included by `examples/release_sign.rs`, which makes the signature at release time.

use ed25519_dalek::{Signature, VerifyingKey};

/// Public half of the release signing key (`cargo run -p spacetree-app --example release_sign -- pubkey`).
pub const PUBLIC_KEY: [u8; 32] = [
    0x07, 0x4d, 0xdb, 0x94, 0x67, 0x48, 0x35, 0x4e, 0x84, 0x78, 0x62, 0x36, 0xab, 0xcd, 0xf9, 0x72, 0x8c, 0x60, 0xef,
    0xdc, 0xd5, 0xb2, 0xd0, 0x29, 0x72, 0xf1, 0x7b, 0xc0, 0xb5, 0x62, 0x16, 0x41,
];

/// The bytes that are signed for `SpaceTree.exe` of `version` whose SHA-256 is `sha256_hex` (lowercase).
pub fn message(version: &str, sha256_hex: &str) -> Vec<u8> {
    format!("SpaceTree release v1\n{version}\n{sha256_hex}\n").into_bytes()
}

pub fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let t = text.trim().as_bytes();
    if t.len() != N * 2 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; N];
    for (o, pair) in out.iter_mut().zip(t.as_chunks::<2>().0) {
        *o = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

/// Checks `sig_text` (128 hex digits, the content of `SpaceTree.exe.sig`) against the built-in key.
pub fn verify_release(version: &str, sha256_hex: &str, sig_text: &str) -> Result<(), String> {
    verify_with(&PUBLIC_KEY, version, sha256_hex, sig_text)
}

pub fn verify_with(key: &[u8; 32], version: &str, sha256_hex: &str, sig_text: &str) -> Result<(), String> {
    let sig = sig_text
        .split_whitespace()
        .next()
        .and_then(from_hex::<64>)
        .ok_or("некорректный файл подписи")?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| "некорректный ключ подписи в программе")?;
    key.verify_strict(&message(version, sha256_hex), &Signature::from_bytes(&sig))
        .map_err(|_| "подпись релиза неверна — обновление отменено".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn sign_hex(version: &str, hash: &str) -> String {
        key()
            .sign(&message(version, hash))
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn valid_signature_is_accepted() {
        let pk = key().verifying_key().to_bytes();
        let sig = sign_hex("1.2.3", HASH);
        assert!(verify_with(&pk, "1.2.3", HASH, &sig).is_ok());
        assert!(verify_with(&pk, "1.2.3", HASH, &format!("{sig}  SpaceTree.exe\n")).is_ok());
    }

    #[test]
    fn signature_is_bound_to_version_and_file() {
        let pk = key().verifying_key().to_bytes();
        let sig = sign_hex("1.2.3", HASH);
        assert!(verify_with(&pk, "1.2.4", HASH, &sig).is_err());
        assert!(verify_with(&pk, "1.2.3", &HASH.replace('0', "1"), &sig).is_err());
    }

    #[test]
    fn foreign_key_and_garbage_are_rejected() {
        let other = SigningKey::from_bytes(&[8u8; 32]).verifying_key().to_bytes();
        let sig = sign_hex("1.2.3", HASH);
        assert!(verify_with(&other, "1.2.3", HASH, &sig).is_err());
        let pk = key().verifying_key().to_bytes();
        assert!(verify_with(&pk, "1.2.3", HASH, "").is_err());
        assert!(verify_with(&pk, "1.2.3", HASH, "zz").is_err());
        assert!(verify_with(&pk, "1.2.3", HASH, &"00".repeat(64)).is_err());
    }

    #[test]
    fn built_in_key_is_a_real_key() {
        assert_ne!(PUBLIC_KEY, [0u8; 32], "release public key is not set");
        assert!(VerifyingKey::from_bytes(&PUBLIC_KEY).is_ok());
    }
}
