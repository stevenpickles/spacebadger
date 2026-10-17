//! Git repositories and linked worktrees found in the scan. Detection
//! reads a few small metadata files per candidate and never runs `git`;
//! see `sb_core::git` for the model and how sizes are counted.

use crate::Metric;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What a listed root is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum GitRootKind {
    /// A checkout with a `.git` folder.
    Repository,
    /// A linked worktree's checkout (`git worktree add`).
    LinkedWorktree,
    /// A submodule's checkout, whose Git directory is in the superproject's
    /// `.git/modules`.
    Submodule,
    /// A checkout whose `.git` file points to a Git directory elsewhere.
    SeparateGitDir,
    /// A bare repository.
    Bare,
    /// A Git directory with no checkout here (separate or submodule).
    GitDir,
    /// Looked like a repository but couldn't be confirmed; see
    /// [`GitRootRow::problem`].
    Unresolved,
}

/// What a node's content is, as far as Git goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum GitContent {
    /// An ordinary repository's, submodule's, or separate Git directory's
    /// checkout.
    Checkout,
    /// A linked worktree's checkout.
    Worktree,
    /// Git's own data.
    Metadata,
}

/// What `HEAD` names, as written there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum GitHead {
    /// A branch, without `refs/heads/`; may not have commits yet.
    Branch { name: String },
    /// Some other ref, in full.
    Ref { name: String },
    /// A commit: the first 12 hex digits of its ID.
    Detached { commit: String },
}

/// A related path, and its node when it's in the scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GitLink {
    pub path: String,
    pub node: Option<u32>,
}

/// One listed repository, worktree, or unresolved candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GitRootRow {
    /// The root's folder; 0 (the scan root) for the repository found above
    /// the scan root (`outside`).
    pub node: u32,
    pub name: String,
    pub path: String,
    pub kind: GitRootKind,
    pub head: Option<GitHead>,
    /// The nearest listed repository around this one.
    pub parent: Option<u32>,
    /// For a linked worktree, the main repository; for a submodule, the
    /// superproject; for a separate Git directory, the Git directory.
    pub repository: Option<GitLink>,
    /// The whole folder as scanned, nested repositories included.
    #[ts(type = "number")]
    pub size: u64,
    /// What only this root accounts for: `size` less nested repositories.
    #[ts(type = "number")]
    pub own: u64,
    /// Git metadata this root accounts for.
    #[ts(type = "number")]
    pub metadata: u64,
    /// What the filter matches in the folder, when the request named one.
    #[ts(type = "number | null")]
    pub matching: Option<u64>,
    #[ts(type = "number")]
    pub files: u64,
    /// Listed roots inside this one.
    pub nested: u32,
    /// Why an unresolved root couldn't be confirmed.
    pub problem: Option<String>,
    /// Where a linked worktree's Git directory says it is, when that isn't
    /// where it was found. Information only.
    pub recorded_at: Option<String>,
    /// Found above the scan root, which is inside it.
    pub outside: bool,
    /// Some of the folder couldn't be read or wasn't scanned.
    pub partial: bool,
}

/// Totals over every root, each byte counted once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GitTotalsWire {
    /// Listed repositories other than linked worktrees and unresolved ones.
    pub roots: u32,
    pub worktrees: u32,
    pub unresolved: u32,
    #[ts(type = "number")]
    pub checkout: u64,
    #[ts(type = "number")]
    pub worktree: u64,
    #[ts(type = "number")]
    pub metadata: u64,
    /// Everything in any repository: the sum of the three above.
    #[ts(type = "number")]
    pub union: u64,
    /// Of `union`, what the filter matches, when the request named one.
    #[ts(type = "number | null")]
    pub matching: Option<u64>,
}

/// Asks for a page of the listed roots, largest first by `metric`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GitRootsRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub metric: Metric,
    /// Adds matching sizes for this search (see `SearchSummary::search`).
    pub search: Option<u32>,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GitRootsPage {
    #[ts(type = "number")]
    pub generation: u64,
    /// The scan revision the roots were read at.
    #[ts(type = "number")]
    pub revision: u64,
    pub offset: u32,
    /// Listed roots in all.
    pub total: u32,
    pub rows: Vec<GitRootRow>,
    pub totals: GitTotalsWire,
    /// The scan is still running, so more roots may be found.
    pub discovering: bool,
}

/// What the details panel shows about a node's place in Git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GitDetails {
    /// `None` outside any repository.
    pub content: Option<GitContent>,
    /// The nearest listed root at or above the node.
    pub root: Option<GitRootRow>,
    /// For a repository, its linked worktrees: registered in its Git
    /// directory, and of those, found in this scan.
    pub worktrees_registered: Option<(u32, u32)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_is_tagged() {
        let json = serde_json::to_value(GitHead::Detached {
            commit: "0123456789ab".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "detached", "commit": "0123456789ab"})
        );
        let json = serde_json::to_value(GitHead::Branch {
            name: "main".into(),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"kind": "branch", "name": "main"}));
    }

    #[test]
    fn root_row_uses_camel_case() {
        let row = GitRootRow {
            node: 4,
            name: "app".into(),
            path: "/w/app".into(),
            kind: GitRootKind::LinkedWorktree,
            head: None,
            parent: None,
            repository: Some(GitLink {
                path: "/w/main".into(),
                node: Some(2),
            }),
            size: 10,
            own: 8,
            metadata: 1,
            matching: None,
            files: 3,
            nested: 0,
            problem: None,
            recorded_at: Some("/old/app".into()),
            outside: false,
            partial: false,
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["kind"], "linkedWorktree");
        assert_eq!(json["recordedAt"], "/old/app");
        assert_eq!(json["repository"]["node"], 2);
        let details = GitDetails {
            content: Some(GitContent::Metadata),
            root: None,
            worktrees_registered: Some((3, 1)),
        };
        let json = serde_json::to_value(&details).unwrap();
        assert_eq!(json["content"], "metadata");
        assert_eq!(json["worktreesRegistered"], serde_json::json!([3, 1]));
    }
}
