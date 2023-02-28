//! Key manager client.
use std::sync::Arc;

use async_trait::async_trait;

use oasis_core_runtime::consensus::beacon::EpochTime;

use crate::{
    api::KeyManagerError,
    crypto::{KeyPair, KeyPairId, Secret, SignedPublicKey, VerifiableSecret},
};

/// Key manager client interface.
#[async_trait]
pub trait KeyManagerClient: Send + Sync {
    /// Clear local key cache.
    ///
    /// This will make the client re-fetch the keys from the key manager.
    fn clear_cache(&self);

    /// Get or create named long-term key pair.
    ///
    /// If the key does not yet exist, the key manager will generate one. If
    /// the key has already been cached locally, it will be retrieved from
    /// cache.
    async fn get_or_create_keys(
        &self,
        key_pair_id: KeyPairId,
        generation: u64,
    ) -> Result<KeyPair, KeyManagerError>;

    /// Get long-term public key for a key pair id.
    async fn get_public_key(
        &self,
        key_pair_id: KeyPairId,
        generation: u64,
    ) -> Result<SignedPublicKey, KeyManagerError>;

    /// Get or create named ephemeral key pair for given epoch.
    ///
    /// If the key does not yet exist, the key manager will generate one. If
    /// the key has already been cached locally, it will be retrieved from
    /// cache.
    async fn get_or_create_ephemeral_keys(
        &self,
        key_pair_id: KeyPairId,
        epoch: EpochTime,
    ) -> Result<KeyPair, KeyManagerError>;

    /// Get ephemeral public key for an epoch and a key pair id.
    async fn get_public_ephemeral_key(
        &self,
        key_pair_id: KeyPairId,
        epoch: EpochTime,
    ) -> Result<SignedPublicKey, KeyManagerError>;

    /// Get a copy of the master secret for replication.
    async fn replicate_master_secret(
        &self,
        generation: u64,
    ) -> Result<VerifiableSecret, KeyManagerError>;

    /// Get a copy of the ephemeral secret for replication.
    async fn replicate_ephemeral_secret(&self, epoch: EpochTime)
        -> Result<Secret, KeyManagerError>;
}

#[async_trait]
impl<T: ?Sized + KeyManagerClient> KeyManagerClient for Arc<T> {
    fn clear_cache(&self) {
        KeyManagerClient::clear_cache(&**self)
    }

    async fn get_or_create_keys(
        &self,
        key_pair_id: KeyPairId,
        generation: u64,
    ) -> Result<KeyPair, KeyManagerError> {
        KeyManagerClient::get_or_create_keys(&**self, key_pair_id, generation).await
    }

    async fn get_public_key(
        &self,
        key_pair_id: KeyPairId,
        generation: u64,
    ) -> Result<SignedPublicKey, KeyManagerError> {
        KeyManagerClient::get_public_key(&**self, key_pair_id, generation).await
    }

    async fn get_or_create_ephemeral_keys(
        &self,
        key_pair_id: KeyPairId,
        epoch: EpochTime,
    ) -> Result<KeyPair, KeyManagerError> {
        KeyManagerClient::get_or_create_ephemeral_keys(&**self, key_pair_id, epoch).await
    }

    async fn get_public_ephemeral_key(
        &self,
        key_pair_id: KeyPairId,
        epoch: EpochTime,
    ) -> Result<SignedPublicKey, KeyManagerError> {
        KeyManagerClient::get_public_ephemeral_key(&**self, key_pair_id, epoch).await
    }

    async fn replicate_master_secret(
        &self,
        generation: u64,
    ) -> Result<VerifiableSecret, KeyManagerError> {
        KeyManagerClient::replicate_master_secret(&**self, generation).await
    }

    async fn replicate_ephemeral_secret(
        &self,
        epoch: EpochTime,
    ) -> Result<Secret, KeyManagerError> {
        KeyManagerClient::replicate_ephemeral_secret(&**self, epoch).await
    }
}
