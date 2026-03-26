use super::traits::IssueTrackerProvider;
use super::types::{
    DepAddInput, EpicCreateInput, EpicUpdateInput, IssueCommentInput, IssueCreateInput,
    IssueListParams, IssueUpdateInput,
};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;
use tokio::process::Command;

const MAX_STDERR_CHARS: usize = 500;

/// Beads (`bd` CLI) issue-tracker provider.
///
/// Shells out to the `bd` binary with `--json` for machine-readable output.
/// Uses `tokio::process::Command` (no shell) to prevent command injection.
pub struct BeadsProvider {
    bd_path: String,
    db_path: Option<String>,
    actor: Option<String>,
    timeout: Duration,
}

impl BeadsProvider {
    pub fn new(
        bd_path: String,
        db_path: Option<String>,
        actor: Option<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            bd_path,
            db_path,
            actor,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Build a `Command` pre-configured with global flags (`--db`, `--actor`).
    fn base_cmd(&self) -> Command {
        let mut cmd = Command::new(&self.bd_path);
        if let Some(ref db) = self.db_path {
            cmd.arg("--db").arg(db);
        }
        if let Some(ref actor) = self.actor {
            cmd.arg("--actor").arg(actor);
        }
        cmd
    }

    /// Run a `bd` subcommand with `--json`, parse stdout as JSON.
    async fn run_bd(&self, args: &[&str]) -> anyhow::Result<Value> {
        let mut cmd = self.base_cmd();
        cmd.args(args).arg("--json");

        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("bd command timed out"))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "bd error (exit {}): {}",
                output.status,
                truncate(&stderr, MAX_STDERR_CHARS)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(serde_json::json!({"ok": true}));
        }
        serde_json::from_str(&stdout)
            .map_err(|e| anyhow::anyhow!("failed to parse bd JSON output: {e}"))
    }

    /// Run a `bd` subcommand that accepts body content via stdin.
    async fn run_bd_with_stdin(&self, args: &[&str], stdin_data: &str) -> anyhow::Result<Value> {
        use tokio::io::AsyncWriteExt;

        let mut cmd = self.base_cmd();
        cmd.args(args)
            .arg("--json")
            .arg("--stdin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_data.as_bytes()).await?;
            // Drop to close stdin so the process can proceed.
        }

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("bd command timed out"))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "bd error (exit {}): {}",
                output.status,
                truncate(&stderr, MAX_STDERR_CHARS)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(serde_json::json!({"ok": true}));
        }
        serde_json::from_str(&stdout)
            .map_err(|e| anyhow::anyhow!("failed to parse bd JSON output: {e}"))
    }

    /// Build create-command args from title and optional flags.
    fn build_create_args<'a>(
        title: &'a str,
        priority: Option<u8>,
        label: Option<&'a str>,
        parent: Option<&'a str>,
        assignee: Option<&'a str>,
        deps: Option<&'a str>,
    ) -> Vec<String> {
        let mut args = vec!["create".to_string(), title.to_string()];
        if let Some(p) = parent {
            args.push("--parent".to_string());
            args.push(p.to_string());
        }
        if let Some(p) = priority {
            args.push("--priority".to_string());
            args.push(p.to_string());
        }
        if let Some(l) = label {
            args.push("--label".to_string());
            args.push(l.to_string());
        }
        if let Some(a) = assignee {
            args.push("--assignee".to_string());
            args.push(a.to_string());
        }
        if let Some(d) = deps {
            args.push("--deps".to_string());
            args.push(d.to_string());
        }
        args
    }

    /// Build update-command args from an issue ID and optional fields.
    fn build_update_args(
        id: &str,
        title: Option<&str>,
        priority: Option<u8>,
        status: Option<&str>,
        label: Option<&str>,
        assignee: Option<&str>,
    ) -> Vec<String> {
        let mut args = vec!["update".to_string(), id.to_string()];
        if let Some(t) = title {
            args.push("--title".to_string());
            args.push(t.to_string());
        }
        if let Some(p) = priority {
            args.push("--priority".to_string());
            args.push(p.to_string());
        }
        if let Some(s) = status {
            args.push("--status".to_string());
            args.push(s.to_string());
        }
        if let Some(l) = label {
            args.push("--label".to_string());
            args.push(l.to_string());
        }
        if let Some(a) = assignee {
            args.push("--assignee".to_string());
            args.push(a.to_string());
        }
        args
    }
}

#[async_trait]
impl IssueTrackerProvider for BeadsProvider {
    // ── Epics ──────────────────────────────────────────────────

    async fn epic_create(&self, input: &EpicCreateInput) -> anyhow::Result<Value> {
        let args = Self::build_create_args(
            &input.title,
            input.priority,
            input.label.as_deref(),
            None, // epics have no parent
            None,
            None,
        );
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        if let Some(ref body) = input.body {
            self.run_bd_with_stdin(&arg_refs, body).await
        } else {
            self.run_bd(&arg_refs).await
        }
    }

    async fn epic_get(&self, epic_id: &str) -> anyhow::Result<Value> {
        self.run_bd(&["show", epic_id]).await
    }

    async fn epic_update(&self, epic_id: &str, input: &EpicUpdateInput) -> anyhow::Result<Value> {
        let args = Self::build_update_args(
            epic_id,
            input.title.as_deref(),
            input.priority,
            input.status.as_deref(),
            input.label.as_deref(),
            None,
        );
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run_bd(&arg_refs).await
    }

    async fn epic_delete(&self, epic_id: &str, reason: Option<&str>) -> anyhow::Result<Value> {
        let reason = reason.unwrap_or("deleted via issue_tracker tool");
        self.run_bd(&["close", epic_id, "--reason", reason]).await
    }

    // ── Issues ─────────────────────────────────────────────────

    async fn issue_create(&self, input: &IssueCreateInput) -> anyhow::Result<Value> {
        let args = Self::build_create_args(
            &input.title,
            input.priority,
            input.label.as_deref(),
            input.parent.as_deref(),
            input.assignee.as_deref(),
            input.deps.as_deref(),
        );
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        if let Some(ref body) = input.body {
            self.run_bd_with_stdin(&arg_refs, body).await
        } else {
            self.run_bd(&arg_refs).await
        }
    }

    async fn issue_get(&self, issue_id: &str) -> anyhow::Result<Value> {
        self.run_bd(&["show", issue_id]).await
    }

    async fn issue_update(
        &self,
        issue_id: &str,
        input: &IssueUpdateInput,
    ) -> anyhow::Result<Value> {
        let args = Self::build_update_args(
            issue_id,
            input.title.as_deref(),
            input.priority,
            input.status.as_deref(),
            input.label.as_deref(),
            input.assignee.as_deref(),
        );
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run_bd(&arg_refs).await
    }

    async fn issue_delete(&self, issue_id: &str, reason: Option<&str>) -> anyhow::Result<Value> {
        let reason = reason.unwrap_or("deleted via issue_tracker tool");
        self.run_bd(&["close", issue_id, "--reason", reason]).await
    }

    async fn issue_next(&self) -> anyhow::Result<Value> {
        self.run_bd(&["ready"]).await
    }

    async fn issue_assign(&self, issue_id: &str, assignee: &str) -> anyhow::Result<Value> {
        self.run_bd(&["update", issue_id, "--assignee", assignee])
            .await
    }

    async fn issue_list(&self, params: &IssueListParams) -> anyhow::Result<Value> {
        let mut args = vec!["list"];
        let status_val;
        let priority_val;
        let assignee_val;
        let parent_val;

        if let Some(ref s) = params.status {
            args.push("--status");
            status_val = s.clone();
            args.push(&status_val);
        }
        if let Some(p) = params.priority {
            args.push("--priority");
            priority_val = p.to_string();
            args.push(&priority_val);
        }
        if let Some(ref a) = params.assignee {
            args.push("--assignee");
            assignee_val = a.clone();
            args.push(&assignee_val);
        }
        if let Some(ref parent) = params.parent {
            args.push("--parent");
            parent_val = parent.clone();
            args.push(&parent_val);
        }

        self.run_bd(&args).await
    }

    async fn issue_comment(&self, input: &IssueCommentInput) -> anyhow::Result<Value> {
        self.run_bd_with_stdin(&["comment", &input.issue_id], &input.body)
            .await
    }

    // ── Dependencies ───────────────────────────────────────────

    async fn dep_add(&self, input: &DepAddInput) -> anyhow::Result<Value> {
        let dep_type = input.dep_type.as_deref().unwrap_or("blocks");
        self.run_bd(&[
            "dep",
            "add",
            &input.from_id,
            &input.to_id,
            "--type",
            dep_type,
        ])
        .await
    }

    async fn dep_remove(&self, from_id: &str, to_id: &str) -> anyhow::Result<Value> {
        self.run_bd(&["dep", "remove", from_id, to_id]).await
    }

    async fn dep_tree(&self, issue_id: &str) -> anyhow::Result<Value> {
        self.run_bd(&["dep", "tree", issue_id]).await
    }

    // ── Sync ───────────────────────────────────────────────────

    async fn sync_push(&self) -> anyhow::Result<Value> {
        self.run_bd(&["dolt", "push"]).await
    }

    async fn sync_pull(&self) -> anyhow::Result<Value> {
        self.run_bd(&["dolt", "pull"]).await
    }

    async fn sync_status(&self) -> anyhow::Result<Value> {
        self.run_bd(&["dolt", "status"]).await
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
