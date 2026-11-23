//! Ephemeral scrub-only encryption-key derivation cache.
//!
//! The filesystem key must not be derived with Argon2id for every one of
//! hundreds of thousands of encrypted inodes. The stored metadata never
//! contains the key: this is an in-memory cache scoped to one scrub call.

use crate::error::Result;
use crate::types::EncryptionConfig;
use std::sync::atomic::{compiler_fence, Ordering};

// Test-only, thread-local accounting avoids sharing state across unrelated
// volume tests that run in parallel.
#[cfg(test)]
thread_local! {
    static DERIVATION_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_derivation_attempts() {
    DERIVATION_ATTEMPTS.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn derivation_attempts() -> usize {
    DERIVATION_ATTEMPTS.with(std::cell::Cell::get)
}

#[derive(Default)]
pub(super) struct FsckKeyCache {
    identity: Option<(String, EncryptionConfig)>,
    key: Option<[u8; 32]>,
}

impl FsckKeyCache {
    pub(super) fn get_or_derive(
        &mut self,
        config: &EncryptionConfig,
        uuid: &str,
        derive: impl FnOnce() -> Result<[u8; 32]>,
    ) -> Result<&[u8; 32]> {
        if self
            .identity
            .as_ref()
            .is_none_or(|(cached_uuid, cached_config)| {
                cached_uuid != uuid || cached_config != config
            })
        {
            // A failed re-derivation must never leave the previous pool's
            // encryption key available under the new configuration.
            self.clear();
            #[cfg(test)]
            DERIVATION_ATTEMPTS.with(|count| count.set(count.get() + 1));
            let key = derive()?;
            self.key = Some(key);
            self.identity = Some((uuid.to_string(), config.clone()));
        }
        Ok(self.key.as_ref().expect("validated scrub key cache"))
    }

    fn clear(&mut self) {
        if let Some(key) = self.key.as_mut() {
            // A normal fill(0) can be eliminated as a dead store by an
            // optimizer; volatile writes and a compiler fence prevent that.
            // This also clears keys when the encryption configuration changes.
            for byte in key.iter_mut() {
                // SAFETY: byte is a valid, uniquely borrowed stack location.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
            compiler_fence(Ordering::SeqCst);
        }
        self.key = None;
        self.identity = None;
    }
}

impl Drop for FsckKeyCache {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ArgosError;

    #[test]
    fn key_is_derived_once_for_many_files_and_invalidated_when_crypto_identity_changes() {
        let mut cache = FsckKeyCache::default();
        let mut config = EncryptionConfig {
            enabled: true,
            ..EncryptionConfig::default()
        };
        let mut derivations = 0usize;
        for _ in 0..1000 {
            let key = cache
                .get_or_derive(&config, "first-pool", || {
                    derivations += 1;
                    Ok([17; 32])
                })
                .unwrap();
            assert_eq!(*key, [17; 32]);
        }
        assert_eq!(derivations, 1, "Argon2id must not run for each inode");

        config.salt_hex = "new-salt".to_string();
        assert_eq!(
            cache
                .get_or_derive(&config, "first-pool", || {
                    derivations += 1;
                    Ok([41; 32])
                })
                .unwrap(),
            &[41; 32]
        );
        assert_eq!(derivations, 2);

        let failure = cache.get_or_derive(&config, "another-pool", || {
            derivations += 1;
            Err(ArgosError::PermissionDenied("wrong key".to_string()))
        });
        assert!(failure.is_err());
        assert!(
            cache.key.is_none(),
            "a failed derivation must clear old key"
        );
        let key = cache
            .get_or_derive(&config, "another-pool", || {
                derivations += 1;
                Ok([99; 32])
            })
            .unwrap();
        assert_eq!(*key, [99; 32]);
        assert_eq!(derivations, 4);
    }
}
