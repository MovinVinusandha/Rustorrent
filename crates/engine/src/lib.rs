//! Core engine library.
//!
//! Protocol modules will be added incrementally behind this crate's public API.

pub mod bencode;
pub mod metainfo;
pub mod storage;

/// Version of the engine API spoken by this build.
pub const API_VERSION: u32 = bt_proto::API_VERSION;
