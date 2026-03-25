pub mod beads;
pub mod traits;
pub mod types;

use crate::security::{policy::ToolOperation, SecurityPolicy};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use traits::IssueTrackerProvider;
use types::{
    DepAddInput, EpicCreateInput, EpicUpdateInput, IssueCreateInput, IssueListParams,
    IssueUpdateInput,
};

const MAX_ERROR_BODY_CHARS: usize = 500;

/// Tool for issue-tracking operations (epics, issues, dependencies, sync).
///
/// Supports 17 actions gated by `[issue_tracker].allowed_actions` in config:
/// - `epic.create`   — create a new epic
/// - `epic.get`      — get epic details
/// - `epic.update`   — update an epic
/// - `epic.delete`   — close/delete an epic
/// - `issue.create`  — create an issue
/// - `issue.get`     — get issue details
/// - `issue.update`  — update an issue
/// - `issue.delete`  — close/delete an issue
/// - `issue.next`    — get the next unblocked issue
/// - `issue.assign`  — assign an issue to a team member
/// - `issue.list`    — list/filter issues
/// - `dep.add`       — add a dependency between issues
/// - `dep.remove`    — remove a dependency
/// - `dep.tree`      — show dependency tree
/// - `sync.push`     — push changes to remote
/// - `sync.pull`     — pull changes from remote
/// - `sync.status`   — show sync status
pub struct IssueTrackerTool {
    provider: Box<dyn IssueTrackerProvider>,
    allowed_actions: Vec<String>,
    security: Arc<SecurityPolicy>,
}

impl IssueTrackerTool {
    pub fn new(
        provider: Box<dyn IssueTrackerProvider>,
        allowed_actions: Vec<String>,
        security: Arc<SecurityPolicy>,
    ) -> Self {
        Self {
            provider,
            allowed_actions,
            security,
        }
    }

    fn is_action_allowed(&self, action: &str) -> bool {
        self.allowed_actions.iter().any(|a| a == action)
    }

    // ── Epic handlers ────────────────────────────────────────────

    async fn handle_epic_create(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let title = require_str(args, "title")?;
        let input = EpicCreateInput {
            title: title.to_string(),
            body: args["body"].as_str().map(String::from),
            priority: args["priority"].as_u64().map(parse_priority),
            label: args["label"].as_str().map(String::from),
        };
        let data = self.provider.epic_create(&input).await?;
        Ok(success_result(data))
    }

    async fn handle_epic_get(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let epic_id = require_str(args, "epic_id")?;
        let data = self.provider.epic_get(epic_id).await?;
        Ok(success_result(data))
    }

    async fn handle_epic_update(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let epic_id = require_str(args, "epic_id")?;
        let input = EpicUpdateInput {
            title: args["title"].as_str().map(String::from),
            body: args["body"].as_str().map(String::from),
            priority: args["priority"].as_u64().map(parse_priority),
            status: args["status"].as_str().map(String::from),
            label: args["label"].as_str().map(String::from),
        };
        let data = self.provider.epic_update(epic_id, &input).await?;
        Ok(success_result(data))
    }

    async fn handle_epic_delete(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let epic_id = require_str(args, "epic_id")?;
        let reason = args["reason"].as_str();
        let data = self.provider.epic_delete(epic_id, reason).await?;
        Ok(success_result(data))
    }

    // ── Issue handlers ───────────────────────────────────────────

    async fn handle_issue_create(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let title = require_str(args, "title")?;
        let input = IssueCreateInput {
            title: title.to_string(),
            parent: args["parent"].as_str().map(String::from),
            body: args["body"].as_str().map(String::from),
            priority: args["priority"].as_u64().map(parse_priority),
            label: args["label"].as_str().map(String::from),
            assignee: args["assignee"].as_str().map(String::from),
            deps: args["deps"].as_str().map(String::from),
        };
        let data = self.provider.issue_create(&input).await?;
        Ok(success_result(data))
    }

    async fn handle_issue_get(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let issue_id = require_str(args, "issue_id")?;
        let data = self.provider.issue_get(issue_id).await?;
        Ok(success_result(data))
    }

    async fn handle_issue_update(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let issue_id = require_str(args, "issue_id")?;
        let input = IssueUpdateInput {
            title: args["title"].as_str().map(String::from),
            body: args["body"].as_str().map(String::from),
            priority: args["priority"].as_u64().map(parse_priority),
            status: args["status"].as_str().map(String::from),
            label: args["label"].as_str().map(String::from),
            assignee: args["assignee"].as_str().map(String::from),
        };
        let data = self.provider.issue_update(issue_id, &input).await?;
        Ok(success_result(data))
    }

    async fn handle_issue_delete(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let issue_id = require_str(args, "issue_id")?;
        let reason = args["reason"].as_str();
        let data = self.provider.issue_delete(issue_id, reason).await?;
        Ok(success_result(data))
    }

    async fn handle_issue_next(&self) -> anyhow::Result<ToolResult> {
        let data = self.provider.issue_next().await?;
        Ok(success_result(data))
    }

    async fn handle_issue_assign(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let issue_id = require_str(args, "issue_id")?;
        let assignee = require_str(args, "assignee")?;
        let data = self.provider.issue_assign(issue_id, assignee).await?;
        Ok(success_result(data))
    }

    async fn handle_issue_list(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let params = IssueListParams {
            status: args["status"].as_str().map(String::from),
            priority: args["priority"].as_u64().map(parse_priority),
            assignee: args["assignee"].as_str().map(String::from),
            parent: args["parent"].as_str().map(String::from),
        };
        let data = self.provider.issue_list(&params).await?;
        Ok(success_result(data))
    }

    // ── Dependency handlers ──────────────────────────────────────

    async fn handle_dep_add(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let from_id = require_str(args, "from_id")?;
        let to_id = require_str(args, "to_id")?;
        let input = DepAddInput {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            dep_type: args["dep_type"].as_str().map(String::from),
        };
        let data = self.provider.dep_add(&input).await?;
        Ok(success_result(data))
    }

    async fn handle_dep_remove(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let from_id = require_str(args, "from_id")?;
        let to_id = require_str(args, "to_id")?;
        let data = self.provider.dep_remove(from_id, to_id).await?;
        Ok(success_result(data))
    }

    async fn handle_dep_tree(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let issue_id = require_str(args, "issue_id")?;
        let data = self.provider.dep_tree(issue_id).await?;
        Ok(success_result(data))
    }

    // ── Sync handlers ────────────────────────────────────────────

    async fn handle_sync_push(&self) -> anyhow::Result<ToolResult> {
        let data = self.provider.sync_push().await?;
        Ok(success_result(data))
    }

    async fn handle_sync_pull(&self) -> anyhow::Result<ToolResult> {
        let data = self.provider.sync_pull().await?;
        Ok(success_result(data))
    }

    async fn handle_sync_status(&self) -> anyhow::Result<ToolResult> {
        let data = self.provider.sync_status().await?;
        Ok(success_result(data))
    }
}

// ── Tool trait impl ─────────────────────────────────────────────

#[async_trait]
impl Tool for IssueTrackerTool {
    fn name(&self) -> &str {
        "issue_tracker"
    }

    fn description(&self) -> &str {
        "Manage epics, issues, dependencies, and team sync via an issue-tracking \
         service. Supports creating, updating, assigning, and closing work items, \
         dependency management, and distributed sync."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "epic.create", "epic.get", "epic.update", "epic.delete",
                        "issue.create", "issue.get", "issue.update", "issue.delete",
                        "issue.next", "issue.assign", "issue.list",
                        "dep.add", "dep.remove", "dep.tree",
                        "sync.push", "sync.pull", "sync.status"
                    ],
                    "description": "The issue-tracker action to perform."
                },
                "issue_id": {
                    "type": "string",
                    "description": "Issue ID (e.g. 'bd-a3f8' or 'bd-a3f8.1')."
                },
                "epic_id": {
                    "type": "string",
                    "description": "Epic ID (e.g. 'bd-a3f8')."
                },
                "title": {
                    "type": "string",
                    "description": "Title for epic/issue create or update."
                },
                "body": {
                    "type": "string",
                    "description": "Description body for epic/issue create or update."
                },
                "priority": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5,
                    "description": "Priority level (1 = highest, 5 = lowest)."
                },
                "status": {
                    "type": "string",
                    "enum": ["open", "in_progress", "closed", "blocked"],
                    "description": "Issue status for update or list filtering."
                },
                "label": {
                    "type": "string",
                    "description": "Label for epic/issue create or update."
                },
                "assignee": {
                    "type": "string",
                    "description": "Assignee name for issue.assign, issue.create, or issue.update."
                },
                "parent": {
                    "type": "string",
                    "description": "Parent epic ID for issue.create or issue.list filtering."
                },
                "deps": {
                    "type": "string",
                    "description": "Comma-separated dependency IDs for issue.create."
                },
                "reason": {
                    "type": "string",
                    "description": "Reason for closing/deleting an epic or issue."
                },
                "from_id": {
                    "type": "string",
                    "description": "Source issue ID for dep.add/dep.remove (the issue that depends)."
                },
                "to_id": {
                    "type": "string",
                    "description": "Target issue ID for dep.add/dep.remove (the issue being depended on)."
                },
                "dep_type": {
                    "type": "string",
                    "enum": ["blocks", "relates_to", "duplicates", "supersedes"],
                    "description": "Dependency type for dep.add. Default: 'blocks'."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: action"))?;

        if !self.is_action_allowed(action) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "action '{action}' is not in the allowed_actions list. \
                     Update [issue_tracker].allowed_actions in config to enable it."
                )),
            });
        }

        // Rate limit check.
        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded for issue_tracker tool.".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action budget exhausted.".into()),
            });
        }

        // Determine operation type.
        let op = match action {
            "epic.get" | "issue.get" | "issue.next" | "issue.list" | "dep.tree"
            | "sync.status" => ToolOperation::Read,
            _ => ToolOperation::Act,
        };
        if let Err(error) = self.security.enforce_tool_operation(op, "issue_tracker") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let result = match action {
            "epic.create" => self.handle_epic_create(&args).await,
            "epic.get" => self.handle_epic_get(&args).await,
            "epic.update" => self.handle_epic_update(&args).await,
            "epic.delete" => self.handle_epic_delete(&args).await,
            "issue.create" => self.handle_issue_create(&args).await,
            "issue.get" => self.handle_issue_get(&args).await,
            "issue.update" => self.handle_issue_update(&args).await,
            "issue.delete" => self.handle_issue_delete(&args).await,
            "issue.next" => self.handle_issue_next().await,
            "issue.assign" => self.handle_issue_assign(&args).await,
            "issue.list" => self.handle_issue_list(&args).await,
            "dep.add" => self.handle_dep_add(&args).await,
            "dep.remove" => self.handle_dep_remove(&args).await,
            "dep.tree" => self.handle_dep_tree(&args).await,
            "sync.push" => self.handle_sync_push().await,
            "sync.pull" => self.handle_sync_pull().await,
            "sync.status" => self.handle_sync_status().await,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("unknown action: {action}")),
                })
            }
        };

        result.or_else(|e| {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(truncate(&e.to_string(), MAX_ERROR_BODY_CHARS)),
            })
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn success_result(data: Value) -> ToolResult {
    ToolResult {
        success: true,
        output: serde_json::to_string_pretty(&data).unwrap_or_default(),
        error: None,
    }
}

fn require_str<'a>(args: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    args[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: {field}"))
}

fn parse_priority(v: u64) -> u8 {
    v.min(255) as u8
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
