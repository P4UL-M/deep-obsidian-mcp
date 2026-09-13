pub mod auth;
pub mod bootstrap;
pub mod couchdb_source;
pub mod edits;
pub mod federation;
pub mod health;
pub mod mcp;
pub mod mounts;
pub mod prompts;
pub mod protocol;
pub mod resources;
pub mod runtime;
pub mod stdio;
pub mod tools;
pub mod uploads;

pub use bootstrap::{run_http_service, ServiceBootstrapContext};
pub use deep_obsidian_config::{
    build_service_endpoints, default_config_path, ensure_http_service_config, normalize_http_path,
    normalize_service_config,
};
pub use deep_obsidian_types::{
    AutoReindexConfig, EmbeddingConfig, HttpConfig, ResolvedServiceConfig, ServiceConfigInput,
    ServiceEndpoints, TransportMode,
};
pub use stdio::run_stdio_service;
