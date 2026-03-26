use serde::{Deserialize, Serialize};

/// Input fields for creating an epic (top-level issue).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpicCreateInput {
    pub title: String,
    pub body: Option<String>,
    /// Priority level 1-5 (1 = highest).
    pub priority: Option<u8>,
    pub label: Option<String>,
}

/// Input fields for updating an epic. All fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpicUpdateInput {
    pub title: Option<String>,
    pub body: Option<String>,
    pub priority: Option<u8>,
    /// Status: open, in_progress, closed, blocked.
    pub status: Option<String>,
    pub label: Option<String>,
}

/// Input fields for creating an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueCreateInput {
    pub title: String,
    /// Parent epic ID to nest this issue under.
    pub parent: Option<String>,
    pub body: Option<String>,
    pub priority: Option<u8>,
    pub label: Option<String>,
    pub assignee: Option<String>,
    /// Comma-separated dependency IDs.
    pub deps: Option<String>,
}

/// Input fields for updating an issue. All fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueUpdateInput {
    pub title: Option<String>,
    pub body: Option<String>,
    pub priority: Option<u8>,
    pub status: Option<String>,
    pub label: Option<String>,
    pub assignee: Option<String>,
}

/// Input fields for adding a comment to an issue or epic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueCommentInput {
    /// The issue or epic ID to comment on.
    pub issue_id: String,
    /// The comment body text.
    pub body: String,
}

/// Parameters for filtering issue list queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssueListParams {
    pub status: Option<String>,
    pub priority: Option<u8>,
    pub assignee: Option<String>,
    /// Filter by parent epic ID.
    pub parent: Option<String>,
}

/// Input fields for adding a dependency between issues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepAddInput {
    /// The issue that depends on another.
    pub from_id: String,
    /// The issue being depended on.
    pub to_id: String,
    /// Dependency type: blocks, relates_to, duplicates, supersedes. Default: blocks.
    pub dep_type: Option<String>,
}
