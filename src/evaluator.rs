//! Per-frame condition evaluator.
//!
//! Owns hit counts, deltas, prior values. Walks each condset applying
//! combining flags. Emits triggered events to the runtime.
