//! Tach — a typed goal runtime for long-horizon agents.
//!
//! Tach gives an agent's goals a typed, deterministic, auditable control plane:
//! goal -> budget -> authority -> diagnostic -> typed patch -> verify -> checkpoint
//! -> resume -> trace.
//!
//! The foundation is a small compiled language whose compiler is also an agent
//! harness: every diagnostic carries a machine-applicable repair (`preferred_patch`),
//! so an agent — even a deterministic one — can drive a failing project to green
//! without guessing. The goal runtime wraps that loop in durability: budgets,
//! authority scopes, append-only event history, checkpoints, resume, and replay.

pub mod action;
pub mod adopt;
pub mod agent;
pub mod ast;
pub mod builtins;
pub mod check;
pub mod cli;
pub mod diagnostics;
pub mod event;
pub mod fmt;
pub mod goal;
pub mod guard;
pub mod interp;
pub mod lexer;
pub mod mcp;
pub mod parser;
pub mod patch;
pub mod plan;
pub mod program;
pub mod project;
pub mod render;
pub mod runner;
pub mod runtime;
pub mod schema;
pub mod shell;
pub mod snapshot;
pub mod source;
pub mod span;
pub mod store;
pub mod term;
pub mod token;
pub mod trace;
pub mod types;
pub mod value;
