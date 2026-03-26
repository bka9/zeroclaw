use super::types::{
    DepAddInput, EpicCreateInput, EpicUpdateInput, IssueCommentInput, IssueCreateInput,
    IssueListParams, IssueUpdateInput,
};
use async_trait::async_trait;
use serde_json::Value;

/// Abstract interface for issue-tracking providers.
///
/// Each method returns provider-native JSON wrapped in `anyhow::Result`.
/// The [`super::IssueTrackerTool`] wrapper converts results into [`ToolResult`].
#[async_trait]
pub trait IssueTrackerProvider: Send + Sync {
    // ── Epics ──────────────────────────────────────────────────
    async fn epic_create(&self, input: &EpicCreateInput) -> anyhow::Result<Value>;
    async fn epic_get(&self, epic_id: &str) -> anyhow::Result<Value>;
    async fn epic_update(&self, epic_id: &str, input: &EpicUpdateInput) -> anyhow::Result<Value>;
    async fn epic_delete(&self, epic_id: &str, reason: Option<&str>) -> anyhow::Result<Value>;

    // ── Issues ─────────────────────────────────────────────────
    async fn issue_create(&self, input: &IssueCreateInput) -> anyhow::Result<Value>;
    async fn issue_get(&self, issue_id: &str) -> anyhow::Result<Value>;
    async fn issue_update(&self, issue_id: &str, input: &IssueUpdateInput)
        -> anyhow::Result<Value>;
    async fn issue_delete(&self, issue_id: &str, reason: Option<&str>) -> anyhow::Result<Value>;
    async fn issue_next(&self) -> anyhow::Result<Value>;
    async fn issue_assign(&self, issue_id: &str, assignee: &str) -> anyhow::Result<Value>;
    async fn issue_list(&self, params: &IssueListParams) -> anyhow::Result<Value>;
    async fn issue_comment(&self, input: &IssueCommentInput) -> anyhow::Result<Value>;

    // ── Dependencies ───────────────────────────────────────────
    async fn dep_add(&self, input: &DepAddInput) -> anyhow::Result<Value>;
    async fn dep_remove(&self, from_id: &str, to_id: &str) -> anyhow::Result<Value>;
    async fn dep_tree(&self, issue_id: &str) -> anyhow::Result<Value>;

    // ── Sync ───────────────────────────────────────────────────
    async fn sync_push(&self) -> anyhow::Result<Value>;
    async fn sync_pull(&self) -> anyhow::Result<Value>;
    async fn sync_status(&self) -> anyhow::Result<Value>;
}
