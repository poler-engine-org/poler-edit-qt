//! poler-edit-keygen — инструмент ИЗДАТЕЛЯ лицензий POLER Edit.
//!
//! Бесполезен без секретного ключа, которого в репозитории НЕТ.
//!
//! Использование:
//! ```text
//! # показать публичный ключ поколения (для вшивания в license.rs):
//! poler-edit-keygen --pub <seed-hex-64>
//! # выпустить ключ:
//! poler-edit-keygen <seed-hex-64> <Имя> <expiry-unix|0>
//! ```
//! seed — 32 байта hex (64 символа). Храните как глазное яблоко.

use ed25519_dalek::SigningKey;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--pub" {
        match seed_from_hex(&args[2]) {
            Some(seed) => {
                let sk = SigningKey::from_bytes(&seed);
                println!("{}", hex(&sk.verifying_key().to_bytes()));
            }
            None => {
                eprintln!("seed: 64 hex-символа (32 байта)");
                std::process::exit(2);
            }
        }
        return;
    }
    if args.len() == 4 {
        let (Some(seed), name, expiry) = (seed_from_hex(&args[1]), &args[2], &args[3]) else {
            eprintln!("seed: 64 hex-символа (32 байта)");
            std::process::exit(2);
        };
        let exp: u32 = match expiry.parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("expiry: unix-секунды или 0 (бессрочная)");
                std::process::exit(2);
            }
        };
        let sk = SigningKey::from_bytes(&seed);
        let key = poler_edit_core::license::sign_key(
            &sk,
            name,
            poler_edit_core::license::EDITION_PRO,
            exp,
        );
        println!("{key}");
        return;
    }
    eprintln!("poler-edit-keygen — выпуск лицензий POLER Edit (Ed25519)");
    eprintln!("  poler-edit-keygen --pub <seed-hex>          публичный ключ поколения");
    eprintln!("  poler-edit-keygen <seed-hex> <Имя> <0|unix> выпуск ключа Pro");
    std::process::exit(1);
}

fn seed_from_hex(h: &str) -> Option<[u8; 32]> {
    let h = h.trim();
    if h.len() != 64 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}
