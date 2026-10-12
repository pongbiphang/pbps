//! The resolver: scratch compilation of the managed declarations, separate
//! from environment and binding qualification (ADR-0016, DEC-1528.1).
//!
//! It reaches its scratch with SQL alone, so nothing here is gated to one
//! platform: a server scratch is a connection string, and a Docker scratch
//! is the `docker` command the runner has, on any platform it runs on.

pub mod scope;

pub mod server;

pub mod sealing;
