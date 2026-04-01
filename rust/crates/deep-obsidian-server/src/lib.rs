pub mod bootstrap;
pub mod compat;
pub mod mcp;
pub mod protocol;
pub mod vault;

pub use bootstrap::{run_http_service, ServiceBootstrapContext};
pub use compat::{node_serve_invocation, NodeServeInvocation};
pub use deep_obsidian_config::{
    build_service_endpoints, default_config_path, ensure_http_service_config, normalize_http_path,
    normalize_service_config,
};
pub use deep_obsidian_types::{
    AutoReindexConfig, EmbeddingConfig, HttpConfig, ResolvedServiceConfig, ServiceConfigInput,
    ServiceEndpoints, TransportMode,
};
