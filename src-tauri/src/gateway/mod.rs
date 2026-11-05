pub(crate) mod probe;
pub(crate) mod request;
mod server;
pub mod translate;
pub mod upstream;

use serde::Serialize;

pub use server::build_router;
pub use upstream::{Ctx, GatewayState, SharedConfig};

#[derive(Debug, Clone, Serialize)]
pub struct GatewayStatus {
    pub running: bool,
    pub port: u16,
    pub base_url: String,
    pub models: Vec<String>,
}

#[cfg(test)]
mod tests;
