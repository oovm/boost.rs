//! `boost-crypto` 固定向量与往返验收。
//!
//! 覆盖：SHA-256/384 NIST KAT、HMAC RFC 4231 case 1/2、HKDF RFC 5869 case 1、
//! AES-128-GCM NIST CAVS（空 AAD 与带 AAD）、X25519 RFC 7748 §6.1、P-256/P-384 签名往返、
//! RSA 夹具验签与 OAEP(SHA-1) 加密往返、`SecretBytes` 的 Debug 打码与 drop 清零。

use boost_crypto::{aead, ecdsa, encoding, hash, mac, rsa, secret::SecretBytes, x25519};

/// RSA 夹具：2048 位公钥（SubjectPublicKeyInfo DER，base64）。
const RSA_SPKI_B64: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvrw2WDmULG0Tae9qCmg9OvZFI/5BnVDzXJufAtO/VwCNxNwztatN7YyvWcBJmDB0NBOJSLwsN2x5JYuOVwWKonW3jCEIkchOc4KyO5ZeZpGGdxFfBUN8e6uLEjEr+/586kMl/WWs1DKUsk2Bqz1+dvKJg6w9C+12fjRaYjvjXaLJ36Jkv1rKMI+PPqQtIHUqOX0pAZZIWWw/64OT3EPXY1ZFua/XqF+noWIFDJoHQknhjphhrN2An4O524HfzRXA43NxwfPUmFn8KH5YzHLvh2J4xxE6GUJAIScC4jikQb3Qb2r9e2VIodYYNlwKPU9XFtzNi673szXfBHoh6tJL4QIDAQAB";
/// 同一夹具的私钥（PKCS#8 DER，base64），只用于验证我们产出的 OAEP 密文可被解回。
const RSA_PKCS8_B64: &str = "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC+vDZYOZQsbRNp72oKaD069kUj/kGdUPNcm58C079XAI3E3DO1q03tjK9ZwEmYMHQ0E4lIvCw3bHkli45XBYqidbeMIQiRyE5zgrI7ll5mkYZ3EV8FQ3x7q4sSMSv7/nzqQyX9ZazUMpSyTYGrPX528omDrD0L7XZ+NFpiO+NdosnfomS/Wsowj48+pC0gdSo5fSkBlkhZbD/rg5PcQ9djVkW5r9eoX6ehYgUMmgdCSeGOmGGs3YCfg7nbgd/NFcDjc3HB89SYWfwofljMcu+HYnjHEToZQkAhJwLiOKRBvdBvav17ZUih1hg2XAo9T1cW3M2LrvezNd8EeiHq0kvhAgMBAAECgf8RseRYyv60IGpsqYls7b7AeZhwfmYX8iqhrM5TtNFLgQGmAcfKUton76VU7kHlnJkrO6TA2rQ/cBDNKuVJrXnvGHyuw2HIJI0iqj1eydR4L4gKA9KM6nXiA0gjp+wstK9iDFi56XTfOtIQuKAPAuV4o4eeb5w6Pw1/J9w6wNOXHBnqaIcA2JWJfO84IgPxciSRyLqtQPqbwr04L3AcrxvlvqfNUnHn5L4gHuWNnC4CIcdwf5cxq89IIlIdk3NYIqnBXZ7EMfJzM8uHslSUpgmLptcAyQNqvOgIf/ped1VPC4COHPeDf3BXtuPy2LSGS6RxCB67QHzimnvzNk8Bf7UCgYEA9A54jJEmupKOR8rG9ILmwn9cgQBdXm9632zva3IlyW3wKX2qzIJ9TBVNxiIqREIdxvyfY631oF3//u3su9UT+h69IddtldnoTYICqoGZPo5ovDpC4ce2LT77GtdUK5ugVD6aQHHeIaGztpjwTfCC/pSphfxAgJemPECJqYALuT0CgYEAyBG7+XhTdJUkniT9uSS15jbfdTn+pVXYEo+PxDLoeN65E6VbTsE5ruEWs5w3W1MLXStG0M5GJtOi9jd0F1rJG3ukynNAk2lMAmCCkw0ZF/miC7lEvIoffnIAq3n6iz5VavGX6XsWfEHZzld/or34W1fnKca03Ae2xThkQCwtX3UCgYEA7FkTPzXWZrpFFZt3fxVBujuy7q7WvPyC5tpNJiWeGGriOJwdiv4Xr1W2QtmVSxMoWSsOhG7UP2Co6E2rg99/ACtCiAJjespTYLQ2Nr9ZE67AGQDnAlXww/KJJN5BAhYtd0hFA4ICV1mMe5myphf0E+0OGtZF9QJnl1P8vOxm7hUCgYEAsnGG3BIjNWdUuqy5lGmWx9eHZRIt+p9fUeazzsWtLSlRn6ErmEmlPkJKsjYR9aVTDG1uAw0H6td2kO6S38W8e4PDTkKPhSs0fSZBMBFNoWGo05YSJat4PZpG2Kq5+XnGS9VhrgB35KoDLxRBphCUNDxp1IwDggBrVupeGBKPAYUCgYB9OTUaNazeOXbMf0OmKlhA9BPgOActqXn9EYyADxlrn4SQxBMaH+hL7ZQ1nZxn8YQbq2kxkAlKVwj1zestAfUw4jE0gy6uY9MsbXHiIXOsdjYxXr3ReGKjgNE4swxYIr4IEGM0KxegQkKj1cJqmCb22nlFvTDWAD2qOFkoM78zXw==";
/// 夹具对 [`RSA_MESSAGE`] 的 PKCS#1 v1.5 + SHA-256 签名（base64）。
const RSA_PKCS1V15_SHA256_B64: &str = "HVeepTbbHJrVwwdm2lNNg0u16z9e9j9cojPF9wkBjzgFvVHZC0JbgG8cAOPDfgA3oxtHljciNyCsXjpw//U+4sCw5WanVOLQ6wyslP7a4ycEXTESHo4jAGhxGR/E2dOnvsdT4I/kWqQmQ3Bx8QzL4tZ86wagTULd3wZ/AJ+7LCBXOlrl3ivyPYs6FEglh7BjYxJbdF+RJhptv4Kk/XBYcZFIA7y9AMIZE+J+guP01mtjJ6TdR7Zn6ehQmbIEnEGgp0AA6qVAHiJ95jhIfZa37uuUIHiUoFR92GOhbVr4ptKKOrNGWQBFPtms6ooeOT2vjI2KUu5NUfAcNiYB4thnZA==";
/// 夹具对 [`RSA_MESSAGE`] 的 PSS + SHA-256 签名，盐长 32（base64）。
const RSA_PSS_SHA256_B64: &str = "ny4i4WVnTLunZNOhBUt7FCHMxvAKHI+emNDkQD9vow355Hy6FBzgHjMPd+kVah+HRIrNHVGUPcUp03TKCwvUbP76YEoIg2KwfZmDh9glHBD1hQKh79j98QPhj0EyJtrahfna6KHAgiaLJ9UubLKtEcYWlxtbsDIO022I2HmlvsNYmUBpsxFMXfe4V4vDkW+xCXToigXEuRRA+idEnc5JbuONVLwQlbwByR2gyJROTquZfT5ICCFzxOdYNwjKm3z32FU8wepRZN65ecPcx04dUvoEoSIudI+jR6MHYYHyDmGh9ztIR3B5w5elttcZNBuO5PnCAckWXI9h1KwaGjfT+Q==";
/// 夹具签名覆盖的固定消息。
const RSA_MESSAGE: &[u8] = b"boost-crypto rsa fixture";

/// 十六进制解码，仅测试使用。
fn hex(text: &str) -> Vec<u8> {
    assert!(text.len() % 2 == 0, "hex length must be even");
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("valid hex")).collect()
}

#[test]
fn sha2_nist_kat() {
    // FIPS 180-4 示例消息 "abc"。
    assert_eq!(hash::sha256(b"abc").as_slice(), hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").as_slice());
    assert_eq!(
        hash::sha384(b"abc").as_slice(),
        hex("cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7").as_slice()
    );
}

#[test]
fn hmac_rfc4231_cases() {
    // RFC 4231 Test Case 1。
    let key = vec![0x0bu8; 20];
    let expected = hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
    let tag = mac::hmac_sha256(&key, b"Hi There").expect("hmac");
    assert_eq!(tag.as_slice(), expected.as_slice());
    mac::hmac_sha256_verify(&key, b"Hi There", &expected).expect("verify");

    // RFC 4231 Test Case 2。
    let expected = hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    let tag = mac::hmac_sha256(b"Jefe", b"what do ya want for nothing?").expect("hmac");
    assert_eq!(tag.as_slice(), expected.as_slice());

    // RFC 4231 Test Case 1，SHA-384 变体。
    let expected = hex("afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6");
    let tag = mac::hmac_sha384(&key, b"Hi There").expect("hmac");
    assert_eq!(tag.as_slice(), expected.as_slice());
}

#[test]
fn hkdf_rfc5869_case1() {
    // RFC 5869 A.1 Test Case 1（SHA-256）。
    let ikm = vec![0x0bu8; 22];
    let salt = hex("000102030405060708090a0b0c");
    let info = hex("f0f1f2f3f4f5f6f7f8f9");
    let expected_prk = hex("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5");
    let expected_okm = hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865");

    let prk = mac::hkdf_extract_sha256(Some(&salt), &ikm);
    assert_eq!(prk.as_slice(), expected_prk.as_slice());

    let mut okm = vec![0u8; 42];
    mac::hkdf_expand_sha256(&prk, &info, &mut okm).expect("expand");
    assert_eq!(okm, expected_okm);

    // 一步式与分步一致。
    let mut one_shot = vec![0u8; 42];
    mac::hkdf_sha256(Some(&salt), &ikm, &info, &mut one_shot).expect("one shot");
    assert_eq!(one_shot, expected_okm);
}

#[test]
fn aes_128_gcm_nist_vectors() {
    // NIST CAVS gcmEncryptExtIV128：空明文与空 AAD。
    let key = hex("11754cd72aec309bf52f7687212e8957");
    let nonce = hex("3c819d9a9bed087615030b65");
    let sealed = aead::aes_128_gcm_seal(&key, &nonce, b"", b"").expect("seal");
    assert_eq!(sealed, hex("250327c674aaf477aef2675748cf6971"));
    assert_eq!(aead::aes_128_gcm_open(&key, &nonce, b"", &sealed).expect("open"), Vec::<u8>::new());

    // NIST CAVS：非空明文、空 AAD。
    let key = hex("7fddb57453c241d03efbed3ac44e371c");
    let nonce = hex("ee283a3fc75575e33efd4887");
    let plaintext = hex("d5de42b461646c255c87bd2962d3b9a2");
    let sealed = aead::aes_128_gcm_seal(&key, &nonce, b"", &plaintext).expect("seal");
    assert_eq!(sealed, hex("2ccda4a5415cb91e135c2a0f78c9b2fdb36d1df9b9d5e596f83e8b7f52971cb3"));
    assert_eq!(aead::aes_128_gcm_open(&key, &nonce, b"", &sealed).expect("open"), plaintext);

    // NIST CAVS：非空明文、非空 AAD。
    let key = hex("c939cc13397c1d37de6ae0e1cb7c423c");
    let nonce = hex("b3d8cc017cbb89b39e0f67e2");
    let plaintext = hex("c3b3c41f113a31b73d9a5cd432103069");
    let aad = hex("24825602bd12a984e0092d3e448eda5f");
    let sealed = aead::aes_128_gcm_seal(&key, &nonce, &aad, &plaintext).expect("seal");
    assert_eq!(sealed, hex("93fe7d9e9bfd10348a5606e5cafa73540032a1dc85f1c9786925a2e71d8272dd"));
    assert_eq!(aead::aes_128_gcm_open(&key, &nonce, &aad, &sealed).expect("open"), plaintext);

    // AAD 篡改必须拒绝。
    let mut bad_aad = aad.clone();
    bad_aad[0] ^= 0x01;
    assert!(aead::aes_128_gcm_open(&key, &nonce, &bad_aad, &sealed).is_err());
}

#[test]
fn x25519_rfc7748_section61() {
    // RFC 7748 §6.1。
    let alice_priv = SecretBytes::new(hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a"));
    let bob_priv = SecretBytes::new(hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb"));
    let alice_pub = hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    let bob_pub = hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
    let shared = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");

    let derived = x25519::public_key(&alice_priv).expect("pubkey");
    assert_eq!(derived.as_slice(), alice_pub.as_slice());
    let derived = x25519::public_key(&bob_priv).expect("pubkey");
    assert_eq!(derived.as_slice(), bob_pub.as_slice());

    let mut alice_pub_bytes = [0u8; 32];
    alice_pub_bytes.copy_from_slice(&alice_pub);
    let mut bob_pub_bytes = [0u8; 32];
    bob_pub_bytes.copy_from_slice(&bob_pub);

    let k_alice = x25519::shared_secret(&alice_priv, &bob_pub_bytes).expect("dh");
    let k_bob = x25519::shared_secret(&bob_priv, &alice_pub_bytes).expect("dh");
    assert_eq!(k_alice.expose(), shared.as_slice());
    assert_eq!(k_bob.expose(), shared.as_slice());
}

#[test]
fn x25519_generated_key_agrees() {
    let a = x25519::generate_secret_key().expect("host entropy");
    let b = x25519::generate_secret_key().expect("host entropy");
    let pa = x25519::public_key(&a).expect("pubkey a");
    let pb = x25519::public_key(&b).expect("pubkey b");
    let k1 = x25519::shared_secret(&a, &pb).expect("dh 1");
    let k2 = x25519::shared_secret(&b, &pa).expect("dh 2");
    assert_eq!(k1.expose(), k2.expose());
    assert_ne!(a.expose(), b.expose());
}

#[test]
fn ecdsa_p256_roundtrip_and_encodings() {
    let message = b"boost-crypto ecdsa p-256 fixed message";
    let key = ecdsa::p256::SigningKey::generate().expect("host entropy");
    let verifier = key.verifying_key();

    let der = key.sign_der(message);
    let fixed = key.sign_fixed(message);
    verifier.verify_der(message, &der).expect("der verify");
    verifier.verify_fixed(message, &fixed).expect("fixed verify");

    // DER 与 fixed-width 互转一致。
    assert_eq!(ecdsa::p256::der_to_fixed(&der).expect("der to fixed"), fixed);
    assert_eq!(ecdsa::p256::fixed_to_der(&fixed).expect("fixed to der"), der);

    // 公钥经 SEC1 导出再解析仍可验签。
    let reparsed = ecdsa::p256::VerifyingKey::from_sec1_bytes(&verifier.to_sec1_bytes()).expect("sec1");
    reparsed.verify_der(message, &der).expect("reparsed verify");

    // 篡改消息与篡改签名都必须拒绝。
    assert!(verifier.verify_der(b"other message", &der).is_err());
    let mut tampered = der.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(verifier.verify_der(message, &tampered).is_err());

    // RFC 6979 确定性 nonce：同一私钥与消息必须产生同一签名。
    assert_eq!(key.sign_fixed(message), fixed);
}

#[test]
fn ecdsa_p384_roundtrip_and_encodings() {
    let message = b"boost-crypto ecdsa p-384 fixed message";
    let key = ecdsa::p384::SigningKey::generate().expect("host entropy");
    let verifier = key.verifying_key();

    let der = key.sign_der(message);
    let fixed = key.sign_fixed(message);
    verifier.verify_der(message, &der).expect("der verify");
    verifier.verify_fixed(message, &fixed).expect("fixed verify");

    assert_eq!(ecdsa::p384::der_to_fixed(&der).expect("der to fixed"), fixed);
    assert_eq!(ecdsa::p384::fixed_to_der(&fixed).expect("fixed to der"), der);

    assert!(verifier.verify_fixed(message, &fixed[..95]).is_err());
    let mut tampered = fixed;
    tampered[0] ^= 0x01;
    assert!(verifier.verify_fixed(message, &tampered).is_err());
}

#[test]
fn rsa_verify_against_fixture_signatures() {
    let spki = encoding::base64_decode(RSA_SPKI_B64).expect("spki");
    let public = rsa::PublicKey::from_spki_der(&spki).expect("public key");

    let pkcs1 = encoding::base64_decode(RSA_PKCS1V15_SHA256_B64).expect("pkcs1 signature");
    public.verify_pkcs1v15_sha256(RSA_MESSAGE, &pkcs1).expect("pkcs1 verify");
    assert!(public.verify_pkcs1v15_sha256(b"other message", &pkcs1).is_err());

    let pss = encoding::base64_decode(RSA_PSS_SHA256_B64).expect("pss signature");
    public.verify_pss_sha256(RSA_MESSAGE, &pss).expect("pss verify");
    assert!(public.verify_pss_sha256(b"other message", &pss).is_err());

    let mut tampered = pss.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(public.verify_pss_sha256(RSA_MESSAGE, &tampered).is_err());
}

#[test]
fn rsa_oaep_sha1_encrypt_roundtrips_with_fixture_key() {
    let spki = encoding::base64_decode(RSA_SPKI_B64).expect("spki");
    let public = rsa::PublicKey::from_spki_der(&spki).expect("public key");
    let message = b"caching_sha2_password payload";

    let first = public.encrypt_oaep_sha1(message).expect("encrypt");
    let second = public.encrypt_oaep_sha1(message).expect("encrypt");
    assert_eq!(first.len(), 256, "OAEP output must be the modulus size");
    assert_ne!(first, second, "OAEP seed must come from fresh host entropy");

    use ::rsa::pkcs8::DecodePrivateKey as _;
    let private_bytes = encoding::base64_decode(RSA_PKCS8_B64).expect("pkcs8");
    let private = ::rsa::RsaPrivateKey::from_pkcs8_der(&private_bytes).expect("private key");
    let opened = private.decrypt(::rsa::Oaep::new::<sha1_rsa::Sha1>(), &first).expect("decrypt");
    assert_eq!(opened, message.as_slice());
}

#[test]
fn secret_bytes_debug_redacts_plaintext() {
    let secret = SecretBytes::new(b"top-secret-value-0123456789".to_vec());
    let text = format!("{secret:?}");
    assert!(!text.contains("top-secret-value"));
    assert!(!text.contains("0123456789"));
}

#[test]
fn encoding_base64_roundtrip() {
    let bytes = hex("00ff10e1b7");
    assert_eq!(encoding::base64_encode(&bytes), "AP8Q4bc=");
    assert_eq!(encoding::base64_encode_no_pad(&bytes), "AP8Q4bc");
    assert_eq!(encoding::base64_decode("AP8Q4bc=").expect("decode"), bytes);
    assert_eq!(encoding::base64_decode_no_pad("AP8Q4bc").expect("decode"), bytes);
}

// —— SecretBytes drop 清零抽查 ——
//
// 用一个记录型全局分配器捕获目标缓冲区，在 dealloc 时刻检查内容是否已全零。
// 选择 1021（素数）长度，避免与测试二进制内其它并发测试的常规分配尺寸相撞。

mod zeroize_on_drop_probe {
    use super::SecretBytes;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    };

    const PROBE_LEN: usize = 1021;
    static WATCHING: AtomicBool = AtomicBool::new(false);
    static WATCH_PTR: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
    static WATCH_LEN: AtomicUsize = AtomicUsize::new(0);
    static ZEROED_AT_DROP: AtomicBool = AtomicBool::new(false);

    struct Tracking;

    unsafe impl GlobalAlloc for Tracking {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() && layout.size() == PROBE_LEN && WATCHING.load(Ordering::SeqCst) && WATCH_PTR.load(Ordering::SeqCst).is_null() {
                WATCH_PTR.store(ptr, Ordering::SeqCst);
                WATCH_LEN.store(layout.size(), Ordering::SeqCst);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if ptr == WATCH_PTR.load(Ordering::SeqCst) {
                let len = WATCH_LEN.load(Ordering::SeqCst);
                let contents = unsafe { core::slice::from_raw_parts(ptr, len) };
                ZEROED_AT_DROP.store(contents.iter().all(|&byte| byte == 0), Ordering::SeqCst);
                WATCH_PTR.store(core::ptr::null_mut(), Ordering::SeqCst);
                WATCHING.store(false, Ordering::SeqCst);
            }
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Tracking = Tracking;

    #[test]
    fn drop_zeroizes_the_heap_buffer() {
        ZEROED_AT_DROP.store(false, Ordering::SeqCst);
        WATCH_PTR.store(core::ptr::null_mut(), Ordering::SeqCst);
        WATCHING.store(true, Ordering::SeqCst);
        {
            let bytes = vec![0xABu8; PROBE_LEN];
            let secret = SecretBytes::new(bytes);
            assert_eq!(secret.len(), PROBE_LEN);
        }
        assert!(WATCH_PTR.load(Ordering::SeqCst).is_null(), "probe buffer dealloc must be observed");
        assert!(ZEROED_AT_DROP.load(Ordering::SeqCst), "drop must zero the buffer before dealloc");
    }
}
