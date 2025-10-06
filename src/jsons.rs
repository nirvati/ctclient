//! Structs for parsing server response.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct STH {
    pub tree_size: u64,
    pub timestamp: u64,
    pub sha256_root_hash: String,
    pub tree_head_signature: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsistencyProof {
    pub consistency: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GetEntries {
    pub entries: Vec<LeafEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LeafEntry {
    pub leaf_input: String,
    pub extra_data: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditProof {
    pub leaf_index: u64,
    pub audit_path: Vec<String>,
}
