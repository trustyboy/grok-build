use std::sync::atomic::{AtomicUsize, Ordering};

use super::{cached_client, client_cache_key};

#[test]
fn cache_key_changes_for_rotated_material_and_protocol() {
    let original = client_cache_key(b"certificate", b"private-key", false);
    assert!(original != client_cache_key(b"certificate-2", b"private-key", false));
    assert!(original != client_cache_key(b"certificate", b"private-key-2", false));
    assert!(original != client_cache_key(b"certificate", b"private-key", true));
    assert_ne!(
        original.identity_digest,
        client_cache_key(b"certificate-2", b"private-key", false).identity_digest
    );
    assert_ne!(
        original.identity_digest,
        client_cache_key(b"certificate", b"private-key-2", false).identity_digest
    );
    assert_eq!(
        original.identity_digest,
        client_cache_key(b"certificate", b"private-key", true).identity_digest
    );
}

#[test]
fn client_pair_digest_tracks_loaded_identity_and_rotation() {
    let sandbox = xai_grok_test_support::TestSandbox::new();
    let cert_dir = sandbox.root();
    let mut previous_digest = None;
    for _ in 0..2 {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let cert_pem = cert.pem();
        let key_pem = key.serialize_pem();
        std::fs::write(cert_dir.join("client.crt"), &cert_pem).unwrap();
        std::fs::write(cert_dir.join("client.key"), &key_pem).unwrap();
        let expected = client_cache_key(cert_pem.as_bytes(), key_pem.as_bytes(), false)
            .identity_digest;
        let (_, digest) =
            super::super::mtls_client_with_identity_digest(cert_dir, false).unwrap();
        assert_eq!(digest, expected);
        if let Some(previous) = previous_digest {
            assert_ne!(digest, previous);
        }
        let (_, cached_digest) =
            super::super::mtls_client_with_identity_digest(cert_dir, false).unwrap();
        assert_eq!(cached_digest, digest);
        let (_, http1_digest) =
            super::super::mtls_client_with_identity_digest(cert_dir, true).unwrap();
        assert_eq!(http1_digest, digest);
        assert!(super::super::mtls_client(cert_dir, false).is_ok());
        previous_digest = Some(digest);
    }
}

#[test]
fn clients_are_reused_for_the_same_identity() {
    static BUILD_CALLS: AtomicUsize = AtomicUsize::new(0);
    let key = client_cache_key(b"cache-test-certificate", b"cache-test-key", false);
    let build = || {
        BUILD_CALLS.fetch_add(1, Ordering::SeqCst);
        reqwest::Client::builder().build()
    };
    assert!(cached_client(key.clone(), build).is_ok());
    assert!(
        cached_client(key, || -> Result<reqwest::Client, reqwest::Error> {
            panic!("cached client must be reused")
        })
        .is_ok()
    );
    assert_eq!(BUILD_CALLS.load(Ordering::SeqCst), 1);
}
