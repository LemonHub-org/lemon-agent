//! Lemon Agent is an unattended, long-running autonomous programming agent.
//!
//! The Rust core provides the scheduler, sandboxed capabilities, event-sourced
//! persistence, budget enforcement, and the evolution engine. Rhai scripts
//! provide replaceable high-level execution strategies and may be improved at
//! runtime; the Rust kernel itself is never modified by the agent.

pub mod cli;
pub mod config;
pub mod error;
pub mod logging;
