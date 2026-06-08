//! Tach — the fast language for coding agents.
//!
//! Tach is a small compiled language whose toolchain is built to cooperate with the
//! agentic coding loop: prompt -> patch -> compile -> test -> repair -> merge.
//!
//! The headline idea: the compiler is also an agent harness. Every diagnostic carries
//! a machine-applicable repair (`preferred_patch`), so an agent — even a deterministic
//! one — can drive a failing project to green without guessing.

pub mod agent;
pub mod ast;
pub mod builtins;
pub mod check;
pub mod cli;
pub mod diagnostics;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod patch;
pub mod program;
pub mod project;
pub mod render;
pub mod runner;
pub mod source;
pub mod span;
pub mod term;
pub mod token;
pub mod trace;
pub mod types;
pub mod value;
