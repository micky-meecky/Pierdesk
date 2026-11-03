//! The unit-test harness cannot act as `ssh-keygen`'s askpass helper, so the passphrase lifecycle
//! is checked here against the real `pierdesk` binary that Cargo builds for integration tests.

#[test]
fn ssh_keygen_gets_passphrases_only_through_the_pierdesk_helper() {
    let askpass = std::path::Path::new(env!("CARGO_BIN_EXE_pierdesk"));
    pierdesk::ssh::keygen_askpass_self_test(askpass).unwrap();
}
