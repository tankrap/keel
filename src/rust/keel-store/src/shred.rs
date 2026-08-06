//! Crypto-shredding for right-to-erasure (Linear NEW-1088).
//!
//! A captured session's sensitive payloads (transcripts, tool outputs) must be *erasable* to satisfy
//! GDPR/CCPA right-to-erasure — but keel's object graph is **immutable and content-addressed**, so we
//! cannot simply delete bytes without breaking the addressing every other object depends on.
//!
//! The resolution is crypto-shredding. A payload is encrypted under a per-key-id data key and the
//! **ciphertext** is held in an erasable side-store keyed by the *plaintext's* BLAKE3 hash; the
//! immutable graph holds only that hash as a pointer. To erase, you delete the data key ([`shred_key`])
//! — every payload under it becomes cryptographically unrecoverable, while the hash pointer stays
//! valid and no other object's integrity is touched. The pointer "dangles" (its content is gone), the
//! address stays valid.
//!
//! Design:
//!   - keyring  (namespace `shredkey`): `key_id → 32-byte data key` — the **erasable** part.
//!   - payloads (namespace `shredct`):  `blake3(plaintext) → [key_id_len:u8][key_id][nonce:12][ct+tag]`.
//!   - cipher: ChaCha20-Poly1305 (AEAD) from `ring`; the plaintext hash is bound as **AAD**, so a
//!     ciphertext can't be moved to a different address and still open. Nonces + keys come from
//!     `ring`'s CSPRNG. We never hand-roll cryptography.
//!
//! Honest limits (documented, not hidden). The contract is: *delete the key ⇒ the payload can never
//! be decrypted again*. That holds cryptographically. What it does NOT by itself guarantee, and what a
//! legal right-to-erasure deployment must handle:
//!   - **Backups / replicas / remotes.** A shred deletes the key only in *this* store. Any copy —
//!     a filesystem backup, a synced replica, a pushed remote — retains recoverable ciphertext **and**
//!     key until it too is shredded. This is the largest scope gap for an erasure claim.
//!   - **Key bytes on disk.** LMDB reuses freed pages lazily, so the deleted key bytes may linger in
//!     the file until overwritten (and a stale MVCC reader can still see them).
//!   - **Key bytes in memory.** The key/nonce buffers and `ring`'s key objects are not zeroized on
//!     drop, so a just-deleted key can persist in freed heap for the process lifetime.
//!
//! A deployment needing the key material *provably* gone should hold the keyring in an external
//! KMS/HSM (and/or add `zeroize` for the in-process buffers). Erasable granularity is the `key_id` —
//! choose it per subject/org so a shred erases exactly the right set.
//!
//! Nonce budget: a fresh random 96-bit nonce per seal under a shared per-`key_id` key. The safe
//! ceiling for random nonces is ~2^32 distinct payloads per `key_id` (birthday bound keeps reuse
//! probability negligible); rotate the `key_id` beyond that. (`ring` offers no XChaCha20/192-bit
//! nonce, so random-96 is the pragmatic choice.)
use crate::object::ObjectId;
use crate::store::{Result, Store, StoreError};
use ring::aead;
use ring::rand::{SecureRandom, SystemRandom};

const NS_KEY: &str = "shredkey"; // the erasable keyring: key_id -> 32-byte data key
const NS_CT: &str = "shredct"; // ciphertext side-store: blake3(plaintext) -> record
const KEY_LEN: usize = 32; // ChaCha20-Poly1305 key
const NONCE_LEN: usize = 12; // 96-bit nonce

/// The outcome of reading a crypto-shreddable payload.
#[derive(Debug, PartialEq, Eq)]
pub enum SecretState {
    /// Recovered plaintext (the data key is present and the ciphertext authenticated).
    Plaintext(Vec<u8>),
    /// The ciphertext exists but its data key was shredded — permanently unrecoverable (erased).
    Shredded,
    /// No payload is stored under this address.
    Absent,
}

fn crypto_err() -> StoreError {
    // ring failures here (RNG/seal) are not object corruption; surface as an IO-class error.
    StoreError::Io(std::io::Error::other("crypto-shred: cipher/RNG operation failed"))
}

impl Store {
    /// Atomically fetch the data key for `key_id`, generating a fresh random one if absent. Uses a
    /// put-if-absent (single write txn) so a concurrent first use of a `key_id` can't have two callers
    /// each generate a different key and clobber each other — which would silently make one caller's
    /// payloads unrecoverable. Returns the key and whether it was just created.
    fn ensure_key(&self, key_id: &str) -> Result<([u8; KEY_LEN], bool)> {
        if key_id.is_empty() || key_id.len() > 255 {
            return Err(StoreError::Io(std::io::Error::other("crypto-shred: key_id must be 1..=255 bytes")));
        }
        let (v, created) = self.aux_get_or_insert(NS_KEY, key_id.as_bytes(), &|| {
            let mut key = vec![0u8; KEY_LEN];
            SystemRandom::new().fill(&mut key).map_err(|_| crypto_err())?;
            Ok(key)
        })?;
        let key = <[u8; KEY_LEN]>::try_from(v.as_slice()).map_err(|_| crypto_err())?;
        Ok((key, created))
    }

    /// Ensure a data key exists for `key_id`, generating a fresh random one if absent. Returns `true`
    /// if a key was created, `false` if one already existed. Idempotent, and safe under concurrent
    /// first use (never overwrites an existing key — see [`ensure_key`]).
    pub fn create_key(&self, key_id: &str) -> Result<bool> {
        Ok(self.ensure_key(key_id)?.1)
    }

    /// Whether a data key currently exists for `key_id` (i.e. its payloads are still recoverable).
    pub fn has_key(&self, key_id: &str) -> Result<bool> {
        Ok(self.aux_get(NS_KEY, key_id.as_bytes())?.is_some())
    }

    fn data_key(&self, key_id: &str) -> Result<Option<[u8; KEY_LEN]>> {
        Ok(self
            .aux_get(NS_KEY, key_id.as_bytes())?
            .and_then(|v| <[u8; KEY_LEN]>::try_from(v.as_slice()).ok()))
    }

    /// Encrypt `plaintext` under `key_id` (creating the key if needed) and store the ciphertext in the
    /// erasable side-store, addressed by `blake3(plaintext)`. Returns that address — the pointer the
    /// immutable graph holds. Storing the same plaintext again is idempotent (same address).
    pub fn put_secret(&self, plaintext: &[u8], key_id: &str) -> Result<ObjectId> {
        let (key, _) = self.ensure_key(key_id)?; // atomic get-or-create; never overwrites
        let id = ObjectId(*blake3::hash(plaintext).as_bytes());

        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| crypto_err())?;
        let ubk = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key).map_err(|_| crypto_err())?;
        let sealing = aead::LessSafeKey::new(ubk);
        let mut buf = plaintext.to_vec();
        // bind the address as AAD: a ciphertext authenticated for `id` can't be opened under another id
        sealing
            .seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::from(id.0), &mut buf)
            .map_err(|_| crypto_err())?;

        let mut rec = Vec::with_capacity(1 + key_id.len() + NONCE_LEN + buf.len());
        rec.push(key_id.len() as u8);
        rec.extend_from_slice(key_id.as_bytes());
        rec.extend_from_slice(&nonce);
        rec.extend_from_slice(&buf);
        self.aux_put(NS_CT, &id.0, &rec)?;
        Ok(id)
    }

    /// Read a crypto-shreddable payload by its address. `Shredded` if the data key is gone (erased),
    /// `Absent` if nothing is stored, `Plaintext` otherwise. A present-key-but-failed-auth is a
    /// tamper/corruption and surfaces as [`StoreError::Corrupt`].
    pub fn read_secret(&self, id: &ObjectId) -> Result<SecretState> {
        let rec = match self.aux_get(NS_CT, &id.0)? {
            Some(r) => r,
            None => return Ok(SecretState::Absent),
        };
        // record: [key_id_len:u8][key_id][nonce:12][ct+tag]
        if rec.is_empty() {
            return Err(StoreError::Corrupt(*id));
        }
        let kl = rec[0] as usize;
        // a well-formed record always has a non-empty key_id (put_secret rejects empty); kl==0 is
        // corruption, not a shredded key — don't let it masquerade as `Shredded`.
        if kl == 0 {
            return Err(StoreError::Corrupt(*id));
        }
        let head = 1 + kl + NONCE_LEN;
        if rec.len() < head + aead::CHACHA20_POLY1305.tag_len() {
            return Err(StoreError::Corrupt(*id));
        }
        let key_id = std::str::from_utf8(&rec[1..1 + kl]).map_err(|_| StoreError::Corrupt(*id))?;
        let key = match self.data_key(key_id)? {
            Some(k) => k,
            None => return Ok(SecretState::Shredded), // key crypto-shredded → unrecoverable
        };
        let nonce: [u8; NONCE_LEN] = rec[1 + kl..head].try_into().map_err(|_| StoreError::Corrupt(*id))?;
        let mut buf = rec[head..].to_vec();
        let ubk = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key).map_err(|_| crypto_err())?;
        let opening = aead::LessSafeKey::new(ubk);
        let plain = opening
            .open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::from(id.0), &mut buf)
            .map_err(|_| StoreError::Corrupt(*id))?; // auth failure with a live key = tampering
        // defense in depth: the recovered plaintext must actually hash to the claimed address
        if *blake3::hash(&plain[..]).as_bytes() != id.0 {
            return Err(StoreError::Corrupt(*id));
        }
        Ok(SecretState::Plaintext(plain.to_vec()))
    }

    /// Crypto-shred `key_id`: delete its data key so every payload encrypted under it becomes
    /// permanently unrecoverable. The ciphertexts are left in place (now inert) — the immutable graph's
    /// pointers stay valid; only the ability to read the content is destroyed. Returns the number of
    /// stored payloads this erased.
    pub fn shred_key(&self, key_id: &str) -> Result<usize> {
        let affected = self
            .aux_iter(NS_CT)?
            .iter()
            .filter(|(_, v)| record_key_id(v) == Some(key_id))
            .count();
        self.aux_delete(NS_KEY, key_id.as_bytes())?;
        Ok(affected)
    }
}

/// The `key_id` a ciphertext record was encrypted under (without decrypting), or `None` if malformed.
fn record_key_id(rec: &[u8]) -> Option<&str> {
    let kl = *rec.first()? as usize;
    let end = 1 + kl;
    if rec.len() < end {
        return None;
    }
    std::str::from_utf8(&rec[1..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    // A unique temp store dir with best-effort cleanup (no rand/time: pid + a counter).
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "keel-shred-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store(t: &Tmp) -> Store {
        Store::open_with_map_size(&t.0, 4 * 1024 * 1024).unwrap()
    }

    #[test]
    fn round_trip_recovers_plaintext_and_addresses_by_hash() {
        let t = Tmp::new();
        let s = store(&t);
        let secret = b"user@example.com asked to deploy prod";
        let id = s.put_secret(secret, "org-1").unwrap();
        assert_eq!(id.0, *blake3::hash(secret).as_bytes(), "addressed by plaintext hash");
        assert_eq!(s.read_secret(&id).unwrap(), SecretState::Plaintext(secret.to_vec()));
    }

    #[test]
    fn identical_plaintext_dedups_to_one_address() {
        let t = Tmp::new();
        let s = store(&t);
        let a = s.put_secret(b"same", "org-1").unwrap();
        let b = s.put_secret(b"same", "org-1").unwrap();
        assert_eq!(a, b, "content-addressed → identical plaintext, identical id");
    }

    #[test]
    fn shredding_the_key_makes_payloads_unrecoverable() {
        let t = Tmp::new();
        let s = store(&t);
        let id1 = s.put_secret(b"secret one", "org-A").unwrap();
        let id2 = s.put_secret(b"secret two", "org-A").unwrap();
        let other = s.put_secret(b"unrelated", "org-B").unwrap();

        let n = s.shred_key("org-A").unwrap();
        assert_eq!(n, 2, "shred reports the number of payloads erased");
        assert!(!s.has_key("org-A").unwrap());
        assert_eq!(s.read_secret(&id1).unwrap(), SecretState::Shredded);
        assert_eq!(s.read_secret(&id2).unwrap(), SecretState::Shredded);
        // a different subject's key is untouched — erasure is scoped to the key_id
        assert_eq!(s.read_secret(&other).unwrap(), SecretState::Plaintext(b"unrelated".to_vec()));
    }

    #[test]
    fn re_creating_a_key_does_not_overwrite_it() {
        // the property the atomic put-if-absent guarantees: a second create_key must NOT mint a new
        // key over the existing one, or already-stored payloads would fail to authenticate.
        let t = Tmp::new();
        let s = store(&t);
        assert!(s.create_key("org-1").unwrap(), "first create mints the key");
        let id = s.put_secret(b"payload under the original key", "org-1").unwrap();
        assert!(!s.create_key("org-1").unwrap(), "second create is a no-op, not a re-mint");
        // if the key had been overwritten, this would be Corrupt (auth failure), not Plaintext
        assert_eq!(
            s.read_secret(&id).unwrap(),
            SecretState::Plaintext(b"payload under the original key".to_vec())
        );
    }

    #[test]
    fn absent_address_reads_as_absent() {
        let t = Tmp::new();
        let s = store(&t);
        let nope = ObjectId(*blake3::hash(b"never stored").as_bytes());
        assert_eq!(s.read_secret(&nope).unwrap(), SecretState::Absent);
    }

    #[test]
    fn tampering_with_ciphertext_is_detected() {
        let t = Tmp::new();
        let s = store(&t);
        let id = s.put_secret(b"authentic payload", "org-1").unwrap();
        // flip a byte in the stored ciphertext (last byte = inside the tag/ct)
        let mut rec = s.aux_get(NS_CT, &id.0).unwrap().unwrap();
        let last = rec.len() - 1;
        rec[last] ^= 0xff;
        s.aux_put(NS_CT, &id.0, &rec).unwrap();
        match s.read_secret(&id) {
            Err(StoreError::Corrupt(bad)) => assert_eq!(bad, id, "auth failure with a live key = corruption"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn keyring_and_payloads_persist_across_reopen() {
        let t = Tmp::new();
        let id = {
            let s = store(&t);
            s.put_secret(b"durable secret", "org-1").unwrap()
        };
        // reopen the same store path — the key and ciphertext must still be there
        let s2 = store(&t);
        assert_eq!(s2.read_secret(&id).unwrap(), SecretState::Plaintext(b"durable secret".to_vec()));
        s2.shred_key("org-1").unwrap();
        drop(s2);
        // and the shred persists across another reopen
        let s3 = store(&t);
        assert_eq!(s3.read_secret(&id).unwrap(), SecretState::Shredded);
    }
}
