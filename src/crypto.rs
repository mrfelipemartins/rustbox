use crate::error::{Error, Result};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};

#[derive(Clone)]
pub struct Crypto(Aes256Gcm);

impl Crypto {
    pub fn new(key: &[u8]) -> Result<Self> {
        Aes256Gcm::new_from_slice(key)
            .map(Self)
            .map_err(|_| Error::Invalid("encryption key must contain 32 bytes".into()))
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .0
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| Error::Internal("encryption failed".into()))?;
        let mut result = nonce.to_vec();
        result.extend(ciphertext);
        Ok(result)
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() < 28 {
            return Err(Error::Internal("invalid encrypted payload".into()));
        }
        self.0
            .decrypt(Nonce::from_slice(&ciphertext[..12]), &ciphertext[12..])
            .map_err(|_| Error::Internal("decryption failed".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encryption_is_authenticated_and_randomized() -> Result<()> {
        let crypto = Crypto::new(&[42; 32])?;
        let first = crypto.encrypt(b"secret")?;
        assert_ne!(first, crypto.encrypt(b"secret")?);
        assert_eq!(crypto.decrypt(&first)?, b"secret");
        let mut altered = first;
        altered[12] ^= 1;
        assert!(crypto.decrypt(&altered).is_err());
        Ok(())
    }
}
