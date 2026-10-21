//! Versioned, authenticated encryption. STREAM binds chunk order and the final
//! chunk; the authenticated header binds the plaintext length to every chunk.
use aead::stream::{DecryptorBE32, EncryptorBE32};
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use aes_siv::siv::Aes256Siv;
use anyhow::{anyhow, bail, ensure, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::{convert::TryInto, path::Path};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

pub const CHUNK: usize = 64 * 1024;
const MAGIC: &[u8; 8] = b"OASISA01";
pub const HEADER: usize = 48; // magic + 256-bit HKDF salt + u64 length

/// Random-access decoding of the existing STREAM format. The chunk index and
/// final flag must match encryption; no plaintext leaves this function unverified.
pub struct FileDecoder {
    header: [u8; HEADER],
    cipher: aead::stream::StreamBE32<Aes256Gcm>,
    pub size: u64,
}

impl FileDecoder {
    pub fn new(key: &[u8; 32], header: [u8; HEADER], encrypted_size: u64) -> Result<Self> {
        use aead::stream::NewStream;
        ensure!(&header[..8] == MAGIC, "Invalid encrypted video header");
        let size = u64::from_be_bytes(header[40..48].try_into()?);
        ensure!(
            size / (CHUNK as u64) < u32::MAX as u64,
            "Encrypted file is too large"
        );
        ensure!(
            plaintext_size(encrypted_size)? == size,
            "Encrypted file framing changed"
        );
        let key = content_key(key, &header[8..40]);
        Ok(Self {
            header,
            cipher: aead::stream::StreamBE32::new((&key).into(), (&[0; 7]).into()),
            size,
        })
    }

    pub fn last_index(&self) -> u64 {
        self.size / CHUNK as u64
    }

    pub fn chunk_size(&self, index: u64) -> usize {
        if index == self.last_index() {
            (self.size % CHUNK as u64) as usize
        } else {
            CHUNK
        }
    }

    pub fn decrypt(&self, index: u64, bytes: &[u8]) -> Result<Vec<u8>> {
        use aead::stream::StreamPrimitive;
        ensure!(
            index <= self.last_index() && bytes.len() == self.chunk_size(index) + 16,
            "Invalid encrypted chunk"
        );
        self.cipher
            .decrypt(
                index.try_into()?,
                index == self.last_index(),
                Payload {
                    msg: bytes,
                    aad: &self.header,
                },
            )
            .map_err(|_| {
                anyhow!("Video authentication failed; data is corrupt or the key is incorrect")
            })
    }
}

fn content_key(key: &[u8; 32], salt: &[u8]) -> [u8; 32] {
    let mut output = [0; 32];
    hkdf::Hkdf::<Sha256>::new(Some(salt), key)
        .expand(b"oasis-content-key-v1", &mut output)
        .unwrap();
    output
}

pub fn parse_key(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64 && value.is_ascii(),
        "Encryption key must be 64 hexadecimal characters (32 bytes)"
    );
    let mut key = [0; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| anyhow!("Encryption key must be hexadecimal"))?;
    }
    Ok(key)
}

pub fn fingerprint(key: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(
        &[b"oasis-storage-key-v1".as_ref(), key].concat(),
    ))
}

pub fn seal(key: &[u8; 32], data: &[u8]) -> Result<String> {
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), data)
        .map_err(|_| anyhow!("Could not encrypt settings"))?;
    Ok(URL_SAFE_NO_PAD.encode([nonce.as_ref(), &encrypted].concat()))
}

pub fn unseal(key: &[u8; 32], data: &str) -> Result<Vec<u8>> {
    let data = URL_SAFE_NO_PAD.decode(data)?;
    ensure!(data.len() >= 28, "Invalid encrypted settings");
    Aes256Gcm::new_from_slice(key)
        .unwrap()
        .decrypt(Nonce::from_slice(&data[..12]), &data[12..])
        .map_err(|_| anyhow!("Could not decrypt settings"))
}

fn name_cipher(key: &[u8; 32]) -> Aes256Siv {
    let mut derived = [0; 64];
    hkdf::Hkdf::<Sha256>::new(None, key)
        .expand(b"oasis-filename-key-v1", &mut derived)
        .unwrap();
    Aes256Siv::new_from_slice(&derived).unwrap()
}

pub fn encrypt_name(key: &[u8; 32], name: &str) -> Result<String> {
    let bytes = name_cipher(key)
        .encrypt([b"oasis-filename-v1"], name.as_bytes())
        .map_err(|_| anyhow!("Filename encryption failed"))?;
    let name = format!(
        "o1-{}",
        data_encoding::BASE32_NOPAD.encode(&bytes).to_lowercase()
    );
    ensure!(
        name.len() <= 255,
        "Encrypted filename is too long (maximum about 140 UTF-8 bytes)"
    );
    Ok(name)
}

pub fn decrypt_name(key: &[u8; 32], name: &str) -> Result<String> {
    let encoded = name
        .strip_prefix("o1-")
        .ok_or_else(|| anyhow!("Unencrypted entry in encrypted storage"))?;
    let bytes = data_encoding::BASE32_NOPAD.decode(encoded.to_uppercase().as_bytes())?;
    let plain = name_cipher(key)
        .decrypt([b"oasis-filename-v1"], &bytes)
        .map_err(|_| anyhow!("Filename authentication failed; check the encryption key"))?;
    Ok(String::from_utf8(plain)?)
}

pub fn plaintext_size(size: u64) -> Result<u64> {
    ensure!(size >= (HEADER + 16) as u64, "Invalid encrypted file size");
    let payload = size - HEADER as u64;
    let full = (payload - 16) / (CHUNK as u64 + 16);
    let last = payload - full * (CHUNK as u64 + 16);
    ensure!(
        (16..CHUNK as u64 + 16).contains(&last),
        "Invalid encrypted file framing"
    );
    Ok(full * CHUNK as u64 + last - 16)
}

pub async fn encrypt_file(key: &[u8; 32], source: &Path, target: &Path) -> Result<()> {
    let mut input = File::open(source).await?;
    let size = input.metadata().await?.len();
    ensure!(
        size / (CHUNK as u64) < u32::MAX as u64,
        "File exceeds encryption format limit"
    );
    let mut salt = [0; 32];
    OsRng.fill_bytes(&mut salt);
    let per_file_key = content_key(key, &salt);
    let nonce = [0; 7];
    let header = [MAGIC.as_ref(), &salt, &size.to_be_bytes()].concat();
    let mut output = File::create(target).await?;
    output.write_all(&header).await?;
    let mut encryptor = EncryptorBE32::<Aes256Gcm>::new((&per_file_key).into(), (&nonce).into());
    let mut left = size;
    while left >= CHUNK as u64 {
        let mut buffer = vec![0; CHUNK];
        input.read_exact(&mut buffer).await?;
        let encrypted = encryptor
            .encrypt_next(Payload {
                msg: &buffer,
                aad: &header,
            })
            .map_err(|_| anyhow!("File encryption failed"))?;
        output.write_all(&encrypted).await?;
        left -= CHUNK as u64;
    }
    let mut buffer = vec![0; left as usize];
    input.read_exact(&mut buffer).await?;
    let encrypted = encryptor
        .encrypt_last(Payload {
            msg: &buffer,
            aad: &header,
        })
        .map_err(|_| anyhow!("File encryption failed"))?;
    output.write_all(&encrypted).await?;
    let mut extra = [0; 1];
    ensure!(
        input.read(&mut extra).await? == 0,
        "Source file changed during encryption"
    );
    output.flush().await?;
    Ok(())
}

pub async fn decrypt_file(key: &[u8; 32], source: &Path, target: &Path) -> Result<()> {
    let mut input = File::open(source).await?;
    let mut header = [0; HEADER];
    input.read_exact(&mut header).await?;
    ensure!(
        &header[..8] == MAGIC,
        "File is not encrypted with the Oasis storage format"
    );
    let size = u64::from_be_bytes(header[40..48].try_into().unwrap());
    ensure!(
        plaintext_size(input.metadata().await?.len())? == size,
        "Encrypted file was truncated or modified"
    );
    let per_file_key = content_key(key, &header[8..40]);
    let nonce = [0; 7];
    let mut decryptor = DecryptorBE32::<Aes256Gcm>::new((&per_file_key).into(), (&nonce).into());
    let mut output = File::create(target).await?;
    let mut left = size;
    while left >= CHUNK as u64 {
        let mut buffer = vec![0; CHUNK + 16];
        input.read_exact(&mut buffer).await?;
        let plain = decryptor
            .decrypt_next(Payload {
                msg: &buffer,
                aad: &header,
            })
            .map_err(|_| {
                anyhow!("File authentication failed; data is corrupt or the key is incorrect")
            })?;
        output.write_all(&plain).await?;
        left -= CHUNK as u64;
    }
    let mut buffer = vec![0; left as usize + 16];
    input.read_exact(&mut buffer).await?;
    let plain = decryptor
        .decrypt_last(Payload {
            msg: &buffer,
            aad: &header,
        })
        .map_err(|_| {
            anyhow!("File authentication failed; data is corrupt or the key is incorrect")
        })?;
    output.write_all(&plain).await?;
    let mut extra = [0; 1];
    if input.read(&mut extra).await? != 0 {
        bail!("Unexpected trailing encrypted data");
    }
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn random_access_chunks_match_full_decryption_and_authenticate_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("ciphertext");
        for size in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, CHUNK * 3] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            tokio::fs::write(&source, &data).await.unwrap();
            encrypt_file(&[11; 32], &source, &target).await.unwrap();
            let bytes = tokio::fs::read(&target).await.unwrap();
            let header: [u8; HEADER] = bytes[..HEADER].try_into().unwrap();
            let decoder = FileDecoder::new(&[11; 32], header, bytes.len() as u64).unwrap();
            for index in (0..=decoder.last_index()).rev() {
                let offset = HEADER + index as usize * (CHUNK + 16);
                let chunk = &bytes[offset..offset + decoder.chunk_size(index) + 16];
                let start = index as usize * CHUNK;
                assert_eq!(
                    decoder.decrypt(index, chunk).unwrap(),
                    data[start..(start + CHUNK).min(size)]
                );
                let mut altered = chunk.to_vec();
                altered[0] ^= 1;
                assert!(decoder.decrypt(index, &altered).is_err());
                assert!(decoder.decrypt(index, &chunk[..chunk.len() - 1]).is_err());
                if index > 0 && chunk.len() == CHUNK + 16 {
                    assert!(decoder.decrypt(index - 1, chunk).is_err());
                }
                let wrong = FileDecoder::new(&[12; 32], header, bytes.len() as u64).unwrap();
                assert!(wrong.decrypt(index, chunk).is_err());
            }
            let mut changed_header = header;
            changed_header[8] ^= 1;
            let wrong = FileDecoder::new(&[11; 32], changed_header, bytes.len() as u64).unwrap();
            assert!(wrong
                .decrypt(0, &bytes[HEADER..HEADER + decoder.chunk_size(0) + 16])
                .is_err());
            assert!(FileDecoder::new(&[11; 32], header, bytes.len() as u64 - 1).is_err());
        }
    }

    #[tokio::test]
    async fn authenticated_round_trips_and_rejects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        let encrypted = dir.path().join("encrypted");
        let decoded = dir.path().join("decoded");
        for size in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, CHUNK * 3] {
            let bytes = vec![42; size];
            tokio::fs::write(&plain, &bytes).await.unwrap();
            encrypt_file(&[7; 32], &plain, &encrypted).await.unwrap();
            assert_eq!(
                plaintext_size(tokio::fs::metadata(&encrypted).await.unwrap().len()).unwrap(),
                size as u64
            );
            decrypt_file(&[7; 32], &encrypted, &decoded).await.unwrap();
            assert_eq!(tokio::fs::read(&decoded).await.unwrap(), bytes);
            assert!(decrypt_file(&[8; 32], &encrypted, &decoded).await.is_err());
            let original = tokio::fs::read(&encrypted).await.unwrap();
            let mut corrupt = original.clone();
            corrupt[HEADER] ^= 1;
            tokio::fs::write(&encrypted, &corrupt).await.unwrap();
            assert!(decrypt_file(&[7; 32], &encrypted, &decoded).await.is_err());
            tokio::fs::write(&encrypted, &original[..original.len() - 1])
                .await
                .unwrap();
            assert!(decrypt_file(&[7; 32], &encrypted, &decoded).await.is_err());
        }
    }

    #[test]
    fn names_are_authenticated_deterministic_and_unicode_safe() {
        for name in ["report.pdf", "a", "照片 résumé.txt"] {
            let encrypted = encrypt_name(&[7; 32], name).unwrap();
            assert_ne!(encrypted, name);
            assert_eq!(encrypted, encrypt_name(&[7; 32], name).unwrap());
            assert_eq!(decrypt_name(&[7; 32], &encrypted).unwrap(), name);
            assert!(decrypt_name(&[8; 32], &encrypted).is_err());
        }
        assert!(parse_key("not a key").is_err());
    }
}
