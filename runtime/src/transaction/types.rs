//! Transaction protocol types.
use std::{
    collections::VecDeque,
    ops::{Deref, DerefMut},
};

use serde_cbor::Value;
use serde_derive::{Deserialize, Serialize};

/// Transaction call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxnCall {
    /// Method name.
    pub method: String,
    /// Method arguments.
    pub args: Value,
}

/// Transaction call output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TxnOutput {
    /// Call invoked successfully.
    Success(Value),
    /// Call raised an error.
    Error(String),
}

/// Internal module to efficiently serialize batches.
mod batch_serialize {
    use serde::{
        de::Deserializer,
        ser::{SerializeSeq, Serializer},
        Deserialize,
    };
    use serde_bytes::{ByteBuf, Bytes};

    pub fn serialize<S>(batch: &Vec<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(batch.len()))?;
        for call in batch {
            seq.serialize_element(&Bytes::new(&call[..]))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<ByteBuf>::deserialize(deserializer).map(|v| v.into_iter().map(|e| e.into()).collect())
    }
}

/// Batch of transaction inputs/outputs.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TxnBatch(#[serde(with = "batch_serialize")] pub Vec<Vec<u8>>);

impl Deref for TxnBatch {
    type Target = Vec<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TxnBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<Vec<u8>>> for TxnBatch {
    fn from(other: Vec<Vec<u8>>) -> TxnBatch {
        TxnBatch(other)
    }
}

impl From<VecDeque<Vec<u8>>> for TxnBatch {
    fn from(other: VecDeque<Vec<u8>>) -> TxnBatch {
        TxnBatch(other.into())
    }
}

impl Into<Vec<Vec<u8>>> for TxnBatch {
    fn into(self) -> Vec<Vec<u8>> {
        self.0.into()
    }
}

impl Into<VecDeque<Vec<u8>>> for TxnBatch {
    fn into(self) -> VecDeque<Vec<u8>> {
        self.0.into()
    }
}