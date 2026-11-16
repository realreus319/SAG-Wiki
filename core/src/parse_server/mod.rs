//! Parse-only server backend (`sag parse-server`, `0.0.0.0:3001`).
//!
//! No desktop indexer, no watcher, no auth. Documents reuse the existing
//! conversion chain (MinerU → anydoc → Docling) through [`job::JobRunner`];
//! audio/video fall back to ffmpeg + ASR and an optional vision model
//! (see [`media`]). Tasks live in an independent `parse.db`
//! ([`store::ParseStore`]); MinerU keys live in the existing
//! `credentials.json` pool ([`keypool`]).
pub mod job;
pub mod keypool;
pub mod media;
pub mod server;
pub mod store;
pub mod types;

#[cfg(test)]
pub mod mock_mineru;

pub use server::run_parse_server;
