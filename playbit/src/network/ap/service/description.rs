//! Bounded discovery is advisory support information, never authorization.
use super::{MAX_FEATURE_NAME_BYTES, MAX_REQUEST_BYTES, MAX_REQUIRED_FEATURES, PROTOCOL};
use serde::{Deserialize, Serialize};
pub const DESCRIPTION_LIMIT: usize = 16 * 1024;
const MAX_ADVERTISED_NAMES: usize = 64;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDescription {
    pub protocol: u8,
    pub operations: Vec<String>,
    pub features: Vec<String>,
    pub algorithms: Algorithms,
    pub limits: Limits,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Algorithms {
    pub kvdb_pull: String,
    pub kvdb_delta: String,
    pub replica_events: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub frame_bytes: usize,
    pub request_bytes: usize,
    pub required_features: usize,
    pub feature_name_bytes: usize,
}
impl ServiceDescription {
    pub fn baseline() -> Self {
        Self {
            protocol: PROTOCOL,
            operations: super::admission::OPERATION_NAMES
                .iter()
                .map(|name| (*name).into())
                .collect(),
            features: super::OPTIONAL_FEATURES.iter().map(|name| (*name).into()).collect(),
            algorithms: Algorithms {
                kvdb_pull: kvdb::sync::PULL_ALGORITHM.into(),
                kvdb_delta: kvdb::sync::DELTA_ALGORITHM.into(),
                replica_events: crate::network::ap::replication::ALGORITHM.into(),
            },
            limits: Limits {
                frame_bytes: crate::network::ap::MAX_FRAME_LEN,
                request_bytes: MAX_REQUEST_BYTES,
                required_features: MAX_REQUIRED_FEATURES,
                feature_name_bytes: MAX_FEATURE_NAME_BYTES,
            },
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.protocol != PROTOCOL
            || self.operations.is_empty()
            || !self.operations.iter().any(|op| op == "describe")
        {
            return Err("invalid AP discovery baseline");
        }
        for (names, feature) in [(&self.operations, false), (&self.features, true)] {
            if names.len() > MAX_ADVERTISED_NAMES {
                return Err("too many AP discovery names");
            }
            for (index, name) in names.iter().enumerate() {
                if !super::admission::valid_name(name, feature) || names[..index].contains(name) {
                    return Err("invalid AP discovery names");
                }
            }
        }
        if self.algorithms.kvdb_pull != "kvdb.pull.v1"
            || self.algorithms.kvdb_delta != "kvdb.delta.v1"
            || self.algorithms.replica_events != "ap.replica.v1"
            || self.limits.frame_bytes == 0
            || self.limits.frame_bytes > crate::network::ap::MAX_FRAME_LEN
            || self.limits.request_bytes == 0
            || self.limits.request_bytes > MAX_REQUEST_BYTES
            || self.limits.required_features > MAX_REQUIRED_FEATURES
            || self.limits.feature_name_bytes == 0
            || self.limits.feature_name_bytes > MAX_FEATURE_NAME_BYTES
        {
            return Err("invalid AP discovery algorithms or limits");
        }
        Ok(())
    }
}
