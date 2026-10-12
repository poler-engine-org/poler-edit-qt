//! poler-edit :: офлайн-валидатор лицензионных ключей (Ed25519).
//!
//! Формат ключа: `PE1-<b64url(payload)>.<b64url(sig64)>`, где
//! payload = `[b'P',b'E'][ver=1][edition: 1=Pro][expiry u32 LE][name_len u8][name utf-8]`.
//! expiry — unix-секунды; 0 = бессрочная («perpetual»).
//! Разделитель — ТОЧКА: base64url-алфавит содержит '-' и '_', но не '.'
//!
//! Свойства (задокументировано честно):
//! * Полностью офлайн: без телеметрии, без серверов, без ревокаций.
//! * Подпись Ed25519 (RFC 8032) — подделка без секретного ключа издателя
//!   вычислительно невозможна;НО опубликованный валидный ключ работает
//!   у любого экземпляра программы — это присущая офлайн-модели цена,
//!   принятая на этом уровне продукта (см. LICENSE.md, Source-Available).
//! * Секретный ключ издателя НЕ находится в репозитории; генерация ключей —
//!   инструментом `poler-edit-keygen` (core/src/bin/keygen.rs).
//!
//! ВАЖНО для интеграции: verify — чистая функция без состояния; активацию
//! хранит GUI (QSettings), ядро только проверяет.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::time::{SystemTime, UNIX_EPOCH};

/// Публичный ключ издателя, поколение 1 (сгенерирован 2026-10-13,
/// секрет у владельца — download/poler-edit-pro-signing-secret.txt,
/// в репозитории его НЕТ). Замена = смена поколения ключей.
pub const PUBLIC_KEY_HEX: &str =
    "308501cbd53188a0d3131e8eb4d7e017430f8863546956246fa65e09f9702b70";

pub const MAGIC: [u8; 2] = *b"PE";
pub const VERSION: u8 = 1;
pub const EDITION_PRO: u8 = 1;

// ---------------------------------------------------------------------
// base64url (без паддинга, алфавит RFC 4648 §5) — без внешних зависимостей
// ---------------------------------------------------------------------

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL[(n >> 18) as usize & 63] as char);
        out.push(B64URL[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64URL[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[n as usize & 63] as char);
        }
    }
    out
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------
// payload / ключ
// ---------------------------------------------------------------------

/// Собрать бинарный payload. Имя длиннее 255 байт обрезается (не падает).
pub fn build_payload(name: &str, edition: u8, expiry: u32) -> Vec<u8> {
    let nb = name.as_bytes();
    let n = nb.len().min(255);
    let mut p = Vec::with_capacity(2 + 1 + 1 + 4 + 1 + n);
    p.extend_from_slice(&MAGIC);
    p.push(VERSION);
    p.push(edition);
    p.extend_from_slice(&expiry.to_le_bytes());
    p.push(n as u8);
    p.extend_from_slice(&nb[..n]);
    p
}

/// Подписать payload секретным ключом издателя и собрать строку ключа.
/// Разделитель '.' — вне base64url-алфавита (грабля №2 спринта: первый
/// вариант с '-' рвался произвольными местами — b64url сам содержит '-').
pub fn sign_key(sk: &SigningKey, name: &str, edition: u8, expiry: u32) -> String {
    let payload = build_payload(name, edition, expiry);
    let sig = sk.sign(&payload);
    format!(
        "PE1-{}.{}",
        b64url_encode(&payload),
        b64url_encode(&sig.to_bytes())
    )
}

#[derive(Clone, Debug)]
pub struct LicenseInfo {
    pub valid: bool,
    pub name: String,
    pub edition: String,
    pub expires: u64,
    pub perpetual: bool,
    pub reason: String,
}

impl LicenseInfo {
    fn invalid(reason: &str) -> LicenseInfo {
        LicenseInfo {
            valid: false,
            name: String::new(),
            edition: "free".into(),
            expires: 0,
            perpetual: false,
            reason: reason.into(),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    let h = hex.trim();
    if h.len() != 64 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Валидация с явным публичным ключом (тесты/эксперименты с поколениями).
pub fn validate_with(pubkey: &[u8; 32], key: &str) -> LicenseInfo {
    let key = key.trim();
    let Some(rest) = key.strip_prefix("PE1-") else {
        return LicenseInfo::invalid("формат: PE1-<данные>.<подпись>");
    };
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return LicenseInfo::invalid("формат: PE1-<данные>.<подпись>");
    }
    let payload = match b64url_decode(parts[0]) {
        Some(p) => p,
        None => return LicenseInfo::invalid("данные ключа не декодируются (base64url)"),
    };
    let sig_bytes = match b64url_decode(parts[1]) {
        Some(s) if s.len() == 64 => s,
        _ => return LicenseInfo::invalid("подпись повреждена"),
    };
    if payload.len() < 9 || payload[0..2] != MAGIC {
        return LicenseInfo::invalid("не ключ POLER Edit");
    }
    if payload[2] != VERSION {
        return LicenseInfo::invalid(&format!("версия ключа {} не поддерживается", payload[2]));
    }
    let edition = payload[3];
    if edition != EDITION_PRO {
        return LicenseInfo::invalid(&format!("неизвестная редакция {edition}"));
    }
    let expiry = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let name_len = payload[8] as usize;
    if payload.len() < 9 + name_len {
        return LicenseInfo::invalid("имя повреждено");
    }
    let name = String::from_utf8_lossy(&payload[9..9 + name_len]).into_owned();

    // Подпись: Ed25519 поверх payload.
    let vk = match VerifyingKey::from_bytes(pubkey) {
        Ok(vk) => vk,
        Err(_) => return LicenseInfo::invalid("публичный ключ издателя повреждён"),
    };
    let mut sb = [0u8; 64];
    sb.copy_from_slice(&sig_bytes);
    let sig = Signature::from_bytes(&sb);
    if vk.verify(&payload, &sig).is_err() {
        return LicenseInfo::invalid("подпись не сошлась (ключ подделан или бит)");
    }
    if expiry != 0 && (now_unix() as u64) > expiry as u64 + 48 * 3600 {
        // 48 часов запаса на часы машины пользователя (офлайн-суверенность).
        return LicenseInfo::invalid("срок действия истёк");
    }
    LicenseInfo {
        valid: true,
        name,
        edition: "pro".into(),
        expires: expiry as u64,
        perpetual: expiry == 0,
        reason: if expiry == 0 {
            "Pro, бессрочная".into()
        } else {
            "Pro".into()
        },
    }
}

/// Валидация вшитым ключом издателя (прод).
pub fn validate_key(key: &str) -> LicenseInfo {
    match parse_hex32(PUBLIC_KEY_HEX) {
        Some(pk) if pk != [0u8; 32] => validate_with(&pk, key),
        _ => LicenseInfo::invalid("ключ издателя не вшит в эту сборку"),
    }
}

/// Публичный ключ издателя из hex (для keygen-вывода и сверки).
pub fn pubkey_from_hex(hex: &str) -> Option<[u8; 32]> {
    parse_hex32(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keypair() -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key().to_bytes();
        (sk, vk)
    }

    #[test]
    fn roundtrip_valid_perpetual() {
        let (sk, vk) = test_keypair();
        let key = sign_key(&sk, "Тест Валідатор", EDITION_PRO, 0);
        let info = validate_with(&vk, &key);
        assert!(info.valid, "reason={}", info.reason);
        assert_eq!(info.name, "Тест Валідатор");
        assert_eq!(info.edition, "pro");
        assert!(info.perpetual);
    }

    #[test]
    fn roundtrip_valid_with_expiry() {
        let (sk, vk) = test_keypair();
        let future = (now_unix() + 86400 * 365) as u32;
        let key = sign_key(&sk, "Годовая лицензия", EDITION_PRO, future);
        let info = validate_with(&vk, &key);
        assert!(info.valid, "reason={}", info.reason);
        assert!(!info.perpetual);
        assert_eq!(info.expires, future as u64);
    }

    #[test]
    fn expired_key_rejected() {
        let (sk, vk) = test_keypair();
        let past = 1_000_000u32; // давно истекла
        let key = sign_key(&sk, "Старая", EDITION_PRO, past);
        let info = validate_with(&vk, &key);
        assert!(!info.valid);
        assert!(info.reason.contains("истёк"), "reason={}", info.reason);
    }

    #[test]
    fn tampered_payload_rejected() {
        let (sk, vk) = test_keypair();
        let key = sign_key(&sk, "Alice", EDITION_PRO, 0);
        // Перекодируем payload с изменённым именем (подпись остаётся старой).
        let stripped = key.strip_prefix("PE1-").unwrap();
        let parts: Vec<&str> = stripped.split('.').collect();
        let payload = b64url_decode(parts[0]).unwrap();
        let mut tampered = payload.clone();
        let last = 9 + payload[8] as usize - 1;
        tampered[last] ^= 0x01; // портим последний байт имени
        let bad = format!("PE1-{}.{}", b64url_encode(&tampered), parts[1]);
        let info = validate_with(&vk, &bad);
        assert!(!info.valid);
        assert!(info.reason.contains("подпись"), "reason={}", info.reason);
    }

    #[test]
    fn wrong_signer_rejected() {
        let (sk, _) = test_keypair();
        let other = SigningKey::from_bytes(&[99u8; 32]);
        let key = sign_key(&sk, "Alice", EDITION_PRO, 0);
        let vk_other = other.verifying_key().to_bytes();
        let info = validate_with(&vk_other, &key);
        assert!(!info.valid);
    }

    #[test]
    fn garbage_inputs_rejected() {
        let (_, vk) = test_keypair();
        for bad in [
            "",
            "PE1",
            "PE1-",
            "PE1-xxx-yyy",
            "PE1-xxx.yyy",
            "XXXX-a.b",
            "PE1-aaa",
            "hello world",
            "PE1-.abc",
            "PE1-abc.",
        ] {
            assert!(!validate_with(&vk, bad).valid, "input={bad:?}");
        }
    }

    #[test]
    fn b64url_roundtrip() {
        for data in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0u8, 255, 3, 7, 8],
        ] {
            let enc = b64url_encode(data);
            assert!(enc.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
            assert_eq!(b64url_decode(&enc).as_deref(), Some(*&data), "data={data:?}");
        }
        assert_eq!(b64url_decode("!!"), None);
    }

    #[test]
    fn prod_demo_key_valid() {
        // Ключ, подписанный боевым поколением ключей издателя (вшит выше):
        // poler-edit-keygen <seed> DEMO 0 — вечный, для ручных проверок.
        let demo = "PE1-UEUBAQAAAAAEREVNTw.PRtAyws4iC3kMYFd8ek_sf6064x0X1xpxkED1DnSvblZ3vq0fPKt9jnDx7qb5Tp5tL5TxGd65wZ9RSTXt6O1CQ";
        let info = validate_key(demo);
        assert!(info.valid, "reason={}", info.reason);
        assert_eq!(info.name, "DEMO");
        assert_eq!(info.edition, "pro");
        assert!(info.perpetual);
        // А против тестового поколения — тот же ключ не сходится.
        let (_, vk_test) = test_keypair();
        assert!(!validate_with(&vk_test, demo).valid);
    }
}
