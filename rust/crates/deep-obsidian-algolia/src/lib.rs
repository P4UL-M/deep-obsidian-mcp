//! Minimal typed Algolia REST client for the shared-wiki feature, plus an
//! in-process mock server used by tests and the demo.
//!
//! There is no official Algolia Rust SDK; this speaks the raw REST API over
//! `reqwest`. The surface is deliberately small: batch writes, search,
//! facet-value search, settings, delete-by-query, get-objects, browse, and
//! secured-key generation.

pub mod client;
pub mod mock;
pub mod records;

pub use client::{
    generate_secured_api_key, AlgoliaClient, AlgoliaError, BrowseResponse, FacetHit,
    SearchRequest, SearchResponse,
};
pub use records::{
    chunk_object_id, note_object_id, ChunkRecord, NoteRecord, RECORD_TYPE_CHUNK, RECORD_TYPE_NOTE,
};
