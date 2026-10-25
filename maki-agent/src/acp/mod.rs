//! Client side of the Agent Client Protocol: drives an external agent process
//! (e.g. `opencode acp`) over stdio NDJSON JSON-RPC. One process hosts many
//! sessions. `maki-acp` serves the other direction, maki as the agent.

mod client;

pub use agent_client_protocol_schema::v1::{InitializeResponse, SessionUpdate};
pub use client::{
    AcpClient, AcpClientError, AcpSession, PermissionPolicy, TurnResult, UpdateHandler,
};
