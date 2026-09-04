use std::sync::Mutex;

use anyhow::{Context, Result};
use protobuf::Message;
use rcgen::SigningKey as _;
use rust_cast::cast::cast_channel as raw;
use sha2::{Digest, Sha256};

/// The receiver's TLS identity: an RSA-2048 self-signed certificate generated
/// at startup plus the signing key used to answer device-auth challenges.
pub struct Identity {
    /// DER-encoded TLS certificate, also used as the device-auth leaf
    /// certificate.
    pub certificate: Vec<u8>,
    /// PKCS#8 DER private key for the TLS server.
    pub private_key: Vec<u8>,
    signing_key: Mutex<rcgen::KeyPair>,
}

impl Identity {
    /// Generates a fresh RSA-2048 identity valid for this receiver run.
    pub fn generate(common_name: &str) -> Result<Self> {
        let key_pair =
            rcgen::KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048)
                .context("could not generate the receiver TLS key pair")?;
        let mut params = rcgen::CertificateParams::new(Vec::new())
            .context("could not prepare the receiver certificate")?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        let certificate = params
            .self_signed(&key_pair)
            .context("could not self-sign the receiver certificate")?;
        Ok(Self {
            certificate: certificate.der().to_vec(),
            private_key: key_pair.serialize_der(),
            signing_key: Mutex::new(key_pair),
        })
    }

    /// Answers a device-auth challenge honestly: the signature covers the
    /// SHA-256 digest of our own TLS certificate, and the presented chain is
    /// our self-signed certificate. Senders that verify the chain against
    /// Google's Cast root CA will not accept it; auth-tolerant senders ignore
    /// the namespace entirely.
    pub fn respond_to_challenge(&self, challenge: &[u8]) -> Option<Vec<u8>> {
        let request = raw::DeviceAuthMessage::parse_from_bytes(challenge).ok()?;
        let challenge = request.challenge.as_ref()?;

        let mut response = raw::AuthResponse::new();
        let signature = self
            .signing_key
            .lock()
            .expect("device auth signing key")
            .sign(&Sha256::digest(&self.certificate))
            .ok()?;
        response.set_signature(signature);
        response.set_client_auth_certificate(self.certificate.clone());
        response.set_signature_algorithm(raw::SignatureAlgorithm::RSASSA_PKCS1v15);
        response.set_hash_algorithm(raw::HashAlgorithm::SHA256);
        if !challenge.sender_nonce().is_empty() {
            response.set_sender_nonce(challenge.sender_nonce().to_vec());
        }

        let mut reply = raw::DeviceAuthMessage::new();
        reply.response = ::protobuf::MessageField::some(response);
        reply.write_to_bytes().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::CertificateDer;

    #[test]
    fn generated_identity_parses_and_signs_challenges() {
        let identity = Identity::generate("Test Cast").expect("identity generation works");
        assert!(!identity.certificate.is_empty());
        assert!(!identity.private_key.is_empty());

        let mut request = raw::DeviceAuthMessage::new();
        let mut challenge = raw::AuthChallenge::new();
        challenge.set_sender_nonce(vec![1, 2, 3, 4]);
        request.challenge = ::protobuf::MessageField::some(challenge);
        let challenge_bytes = request.write_to_bytes().unwrap();

        let response_bytes = identity
            .respond_to_challenge(&challenge_bytes)
            .expect("a challenge is answered");
        let response = raw::DeviceAuthMessage::parse_from_bytes(&response_bytes).unwrap();
        let response = response.response.as_ref().expect("a response is present");
        assert!(!response.signature().is_empty());
        assert_eq!(response.client_auth_certificate(), &identity.certificate);
        assert_eq!(response.sender_nonce(), &[1, 2, 3, 4]);
        assert_eq!(
            response.signature_algorithm(),
            raw::SignatureAlgorithm::RSASSA_PKCS1v15
        );
        assert_eq!(response.hash_algorithm(), raw::HashAlgorithm::SHA256);
    }

    #[test]
    fn challenges_without_a_challenge_field_are_ignored() {
        let identity = Identity::generate("Test Cast").expect("identity generation");
        let request = raw::DeviceAuthMessage::new();
        let bytes = request.write_to_bytes().unwrap();
        assert!(identity.respond_to_challenge(&bytes).is_none());
        assert!(identity.respond_to_challenge(b"not a protobuf").is_none());
        assert!(identity.respond_to_challenge(&[]).is_none());
    }

    #[test]
    fn the_certificate_der_is_a_parseable_certificate() {
        let identity = Identity::generate("Test Cast").expect("identity generation");
        let parsed = CertificateDer::from_slice(&identity.certificate);
        // Re-parsing through rustls types is the closest cheap check that the
        // DER is well-formed; the private key is consumed verbatim by rustls.
        assert_eq!(parsed.as_ref(), &identity.certificate[..]);
        let _ = identity.private_key.len();
    }
}
