//! deep-code benchmark evaluation driver.
//!
//! Provides `run_bench()` which loads a benchmark dataset, drives the
//! coding agent against each instance, and returns a structured report.
//!
//! # Example
//!
//! ```ignore
//! use deep_code_eval::{EvalConfig, load_bench, run_bench};
//!
//! let config = EvalConfig::default();
//! let bench = load_bench("swe-bench", "lite", Some(5)).await?;
//! let report = run_bench(config, &bench).await?;
//! println!("resolved: {}/{}", report.resolved, report.total);
//! ```

pub mod bench;
pub mod report;
pub mod runner;

pub use bench::{BenchmarkInstance, BenchmarkSet, load_bench};
pub use runner::{BenchReport, EvalConfig, InstanceResult, InstanceStatus, run_bench};
