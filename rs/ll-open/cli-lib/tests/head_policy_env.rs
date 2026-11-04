//! `HeadPolicy::from_env` reads the operator's head trust configuration (bead
//! `ley-line-open-b68fa6`). This is its own test binary, so setting the trust
//! variables here cannot race a library test that reads them.

use leyline_cli_lib::head_claim::HeadPolicy;
use leyline_sign::root_signer::Ed25519RootSigner;

#[test]
fn head_policy_reads_the_trust_set_and_require_flag_from_the_environment() {
    let pk = Ed25519RootSigner::from_seed(&[5u8; 32]).verifying_key();
    let pk_hex: String = pk.as_bytes().iter().map(|b| format!("{b:02x}")).collect();

    // SAFETY: the only test in this binary, so nothing else reads the
    // environment concurrently.
    unsafe {
        std::env::remove_var("LEYLINE_HEAD_TRUSTED_KEYS");
        std::env::remove_var("LEYLINE_HEAD_REQUIRE_SIGNATURE");
    }
    let none = HeadPolicy::from_env().unwrap();
    assert!(none.trusted.is_empty());
    assert!(!none.require_signature);

    unsafe {
        std::env::set_var("LEYLINE_HEAD_TRUSTED_KEYS", &pk_hex);
        std::env::set_var("LEYLINE_HEAD_REQUIRE_SIGNATURE", "1");
    }
    let set = HeadPolicy::from_env().unwrap();
    assert_eq!(set.trusted, vec![pk]);
    assert!(set.require_signature);

    unsafe { std::env::set_var("LEYLINE_HEAD_TRUSTED_KEYS", "not-a-key") };
    assert!(
        HeadPolicy::from_env().is_err(),
        "a malformed trust set is an error, never an empty one"
    );
    unsafe {
        std::env::remove_var("LEYLINE_HEAD_TRUSTED_KEYS");
        std::env::remove_var("LEYLINE_HEAD_REQUIRE_SIGNATURE");
    }
}
