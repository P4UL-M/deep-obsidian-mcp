pub mod bootstrap;
pub mod config;
pub mod mcp;
pub mod protocol;
pub mod vault;

pub use bootstrap::{build_endpoints, run_http_service, ServiceBootstrapContext, ServiceEndpoints};
pub use config::{
    normalize_http_path, normalize_service_config, AutoReindexConfig, EmbeddingConfig, HttpConfig,
    ResolvedServiceConfig, ServiceConfigInput, TransportMode,
};

