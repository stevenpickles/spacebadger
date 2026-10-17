//! Scan model, size accounting, aggregation, filename search, treemap
//! layout, comparison with earlier scans, and the Git repositories found in
//! a scan. Independent of any platform API or desktop window.

pub mod compare;
pub mod export;
pub mod filetype;
pub mod git;
pub mod layout;
pub mod ranking;
pub mod search;
pub mod selection;
pub mod size;
pub mod snapshot;
pub mod suggest;
pub mod tree;
