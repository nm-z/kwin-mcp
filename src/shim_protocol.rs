/// A shim-owned server retires after TTL cleanup; standalone servers remain connected.
pub(crate) const RETIRE_ON_TTL_ENV: &str = "KWIN_MCP_RETIRE_ON_TTL";
