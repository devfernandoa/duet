//! Milestone 3's orchestration layer: agent identity, the registry/
//! permissions/message-bus services that route and authorize agent-to-agent
//! messages, provider adapters, activity detection, and the launch
//! environment agents discover `duetctl` through. Sits strictly below GTK in
//! CLAUDE.md's dependency direction (`Domain model -> Application services
//! -> Runtime/integrations -> GTK UI / CLI`): nothing in this module imports
//! `gtk4`/`libadwaita`/`vte4`, so it's exercised directly by unit tests (with
//! `adapter::FakeAgentAdapter`) and by `control.rs`'s CLI dispatch, not only
//! through the GTK app.

pub mod activity;
pub mod adapter;
pub mod bus;
pub mod env;
pub mod identity;
pub mod notes;
pub mod permissions;
pub mod portal;
pub mod registry;
pub mod resource;
pub mod skill;

pub use adapter::{AgentAdapter, adapter_for};
pub use bus::MessageBus;
pub use identity::{AgentIdentity, agent_identities};
pub use permissions::authorize;
pub use registry::AgentRegistry;
pub use resource::{ResolveContext, ResolveOutcome, ResolvedResource, ResourceKind, ResourceRef};
