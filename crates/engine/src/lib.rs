//! Core engine library.
//!
//! Protocol modules will be added incrementally behind this crate's public API.

#![forbid(unsafe_code)]

pub mod bencode;
pub mod bitfield;
pub mod metainfo;
pub mod storage;

/// Version of the engine API spoken by this build.
pub const API_VERSION: u32 = bt_proto::API_VERSION;
