use std::{
    fs,
    io::{self, Write as _},
    path::Path,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use thiserror::Error;
use zeroize::Zeroizing;

const KEY_FILE_VERSION: u8 = 1;

#[derive(Clone)]
pub struct DeviceIdentity {
    signing_key: SigningKey,
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("could not access the identity file: {0}")]
    Io(#[from] io::Error),
    #[error("the identity file is invalid")]
    InvalidFile,
    #[error("the public key is invalid")]
    InvalidPublicKey,
    #[error("the signature is invalid")]
    InvalidSignature,
}

impl DeviceIdentity {
    #[must_use]
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn load_or_create(path: &Path) -> Result<Self, IdentityError> {
        match fs::read(path) {
            Ok(bytes) => Self::from_key_file(&bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate();
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut options = fs::OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;
                    options.mode(0o600);
                }
                let mut file = match options.open(path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        return Self::from_key_file(&fs::read(path)?);
                    }
                    Err(error) => return Err(error.into()),
                };
                file.write_all(&identity.key_file_bytes())?;
                file.sync_all()?;
                Ok(identity)
            }
            Err(error) => Err(error.into()),
        }
    }

    #[must_use]
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    #[must_use]
    pub fn public_key_text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public_key_bytes())
    }

    pub fn decode_public_key_text(value: &str) -> Result<[u8; 32], IdentityError> {
        URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| IdentityError::InvalidPublicKey)?
            .try_into()
            .map_err(|_| IdentityError::InvalidPublicKey)
    }

    #[must_use]
    pub fn secret_key_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.signing_key.to_bytes())
    }

    #[must_use]
    pub fn sign(&self, payload: &[u8]) -> [u8; 64] {
        self.signing_key.sign(payload).to_bytes()
    }

    pub fn verify(
        public_key: &[u8; 32],
        payload: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), IdentityError> {
        let key =
            VerifyingKey::from_bytes(public_key).map_err(|_| IdentityError::InvalidPublicKey)?;
        key.verify(payload, &Signature::from_bytes(signature))
            .map_err(|_| IdentityError::InvalidSignature)
    }

    fn key_file_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(33));
        bytes.push(KEY_FILE_VERSION);
        bytes.extend_from_slice(&self.signing_key.to_bytes());
        bytes
    }

    fn from_key_file(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() != 33 || bytes[0] != KEY_FILE_VERSION {
            return Err(IdentityError::InvalidFile);
        }
        let secret: [u8; 32] = bytes[1..]
            .try_into()
            .map_err(|_| IdentityError::InvalidFile)?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&secret),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_payload() {
        let identity = DeviceIdentity::generate();
        let payload = b"nexo protocol";
        let signature = identity.sign(payload);

        DeviceIdentity::verify(&identity.public_key_bytes(), payload, &signature)
            .expect("valid signature should verify");
        assert!(
            DeviceIdentity::verify(&identity.public_key_bytes(), b"changed", &signature).is_err()
        );
    }

    #[test]
    fn key_file_round_trip() {
        let identity = DeviceIdentity::generate();
        let restored = DeviceIdentity::from_key_file(&identity.key_file_bytes())
            .expect("generated key file should load");
        assert_eq!(identity.public_key_bytes(), restored.public_key_bytes());
    }
}
