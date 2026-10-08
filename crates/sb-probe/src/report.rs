//! Report helpers shared by the platform probes.

use serde::Serialize;
use std::collections::BTreeMap;

/// The result of one metadata call, serialized as `{"ok": ...}` or
/// `{"error": "..."}` so failures are recorded rather than aborting a probe.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome<T> {
    Ok(T),
    Error(String),
}

impl<T, E: std::fmt::Display> From<Result<T, E>> for Outcome<T> {
    fn from(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(err) => Self::Error(err.to_string()),
        }
    }
}

/// Counts and byte totals per category, sorted by name for stable output.
#[derive(Debug, Default, Serialize)]
pub struct Tally(BTreeMap<String, TallyEntry>);

#[derive(Debug, Default, Serialize)]
pub struct TallyEntry {
    pub count: u64,
    pub bytes: u64,
}

impl Tally {
    pub fn add(&mut self, category: impl Into<String>, bytes: u64) {
        let entry = self.0.entry(category.into()).or_default();
        entry.count += 1;
        entry.bytes = entry.bytes.saturating_add(bytes);
    }
}

/// A bounded set of example records per category, plus how many occurred.
#[derive(Debug, Serialize)]
pub struct Samples<T> {
    limit: usize,
    total: BTreeMap<String, u64>,
    examples: BTreeMap<String, Vec<T>>,
}

impl<T> Samples<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            total: BTreeMap::new(),
            examples: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, category: &str, make: impl FnOnce() -> T) {
        *self.total.entry(category.to_owned()).or_default() += 1;
        let list = self.examples.entry(category.to_owned()).or_default();
        if list.len() < self.limit {
            list.push(make());
        }
    }
}
