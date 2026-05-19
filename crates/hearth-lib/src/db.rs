//! Database engine supervision (MySQL/MariaDB, Postgres, Redis).
//!
//! Each engine module exposes a `resolve_*_binary` chain (mirroring `mailpit.rs`)
//! and a `managed_service` builder that returns a `ManagedService` ready for
//! supervisor registration in `service::manager::default_services`.
//!
//! Common scaffolding lives in:
//! - [`health`] — TCP probes for port-in-use detection and `hearth db status`
//! - [`init`]   — wrapper-script harness with atomic init sentinel + flock
//!
//! Engine resolvers/builders are added per-block (Postgres → Redis → MySQL).

pub mod health;
pub mod init;
