//! Release signing tool (run by `scripts/release.ps1`).
//!
//!   release_sign pubkey <keyfile>                    prints the public key as a Rust array for release_sig.rs
//!   release_sign sign <keyfile> <version> <exe>      prints the signature (hex) for SpaceTree.exe.sig
//!
//! `keyfile` holds the 32-byte secret seed as 64 hex digits (`scripts/make_release_key.ps1` creates it).

#[allow(dead_code)]
#[path = "../src/release_sig.rs"]
mod release_sig;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

fn load_key(path: &str) -> Result<SigningKey, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let seed = release_sig::from_hex::<32>(&text).ok_or("the key file must hold 64 hex digits")?;
    Ok(SigningKey::from_bytes(&seed))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["pubkey", key] => {
            let pk = load_key(key)?.verifying_key().to_bytes();
            let items: Vec<String> = pk.iter().map(|b| format!("0x{b:02x}")).collect();
            println!("[{}]", items.join(", "));
        }
        ["sign", key, version, exe] => {
            let bytes = std::fs::read(exe).map_err(|e| format!("{exe}: {e}"))?;
            let hash: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            let sig = load_key(key)?.sign(&release_sig::message(version, &hash));
            let hex: String = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
            print!("{hex}");
        }
        _ => return Err("usage: release_sign pubkey <keyfile> | sign <keyfile> <version> <exe>".into()),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
