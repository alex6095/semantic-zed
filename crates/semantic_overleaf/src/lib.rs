//! Native Overleaf protocol and replica primitives for Semantic Zed.
//!
//! Overleaf's current realtime service still speaks the legacy Socket.IO 0.9
//! framing used by its web client. This crate keeps that protocol, ShareJS OT,
//! and history-OT handling in Rust so the editor does not require a Node.js
//! process for its core collaboration path.

pub mod browser;
pub mod credentials;
pub mod http;
pub mod merge;
pub mod ot;
pub mod ownership;
pub mod project_model;
pub mod realtime;
pub mod replica;
pub mod scan;
pub mod socket_client;
pub mod socket_io;
pub mod sync;
