use super::traits::{Tool, ToolResult};
use crate::security::{policy::ToolOperation, SecurityPolicy};
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

const X_API_BASE: &str = "https://api.x.com/2";
const X_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Maximum number of characters to include from an error response body.
const MAX_ERROR_BODY_CHARS: usize = 500;
/// Duration to cache usage data before re-fetching.
const USAGE_CACHE_TTL_SECS: u64 = 300;

/// Cached usage data from the X API `/2/usage/tweets` endpoint.
struct UsageData {
    project_usage: u64,
    project_cap: u64,
    cap_reset_day: u8,
}

/// Tool for interacting with the X (Twitter) API v2 — list tweets, search,
/// create posts, reply, follow/unfollow users, and monitor API usage.
/// Each action is gated by `allowed_actions` and the appropriate security
/// operation (Read for queries, Act for mutations).
pub struct XTool {
    bearer_token: String,
    access_token: String,
    user_id: String,
    allowed_actions: Vec<String>,
    http: reqwest::Client,
    security: Arc<SecurityPolicy>,
    monthly_tweet_cap: Option<u64>,
    warn_at_percent: u8,
    check_before_action: bool,
    cached_usage: Mutex<Option<(Instant, UsageData)>>,
}

impl XTool {
    /// Create a new X tool.
    ///
    /// - `bearer_token` — app-only auth for read endpoints and usage.
    /// - `access_token` — OAuth 2.0 user token for write endpoints.
    /// - `user_id` — the authenticated user's numeric X ID.
    /// - `allowed_actions` — which actions the agent may call.
    /// - `monthly_tweet_cap` — local cap on tweet reads (if set, enforced via usage API).
    /// - `warn_at_percent` — log warning when usage hits this % of cap.
    /// - `check_before_action` — whether to query usage API before write actions.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bearer_token: String,
        access_token: String,
        user_id: String,
        allowed_actions: Vec<String>,
        security: Arc<SecurityPolicy>,
        monthly_tweet_cap: Option<u64>,
        warn_at_percent: u8,
        check_before_action: bool,
    ) -> Self {
        Self {
            bearer_token,
            access_token,
            user_id,
            allowed_actions,
            http: reqwest::Client::new(),
            security,
            monthly_tweet_cap,
            warn_at_percent,
            check_before_action,
            cached_usage: Mutex::new(None),
        }
    }

    fn is_action_allowed(&self, action: &str) -> bool {
        self.allowed_actions.iter().any(|a| a == action)
    }

    /// Build Authorization header for Bearer token (read endpoints + usage).
    fn bearer_auth(&self) -> anyhow::Result<reqwest::header::HeaderMap> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Authorization",
            format!("Bearer {}", self.bearer_token)
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid X bearer token header value: {e}"))?,
        );
        Ok(headers)
    }

    /// Build Authorization header for OAuth 2.0 user token (write endpoints).
    fn oauth_auth(&self) -> anyhow::Result<reqwest::header::HeaderMap> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Authorization",
            format!("Bearer {}", self.access_token)
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid X access token header value: {e}"))?,
        );
        headers.insert("Content-Type", "application/json".parse().unwrap());
        Ok(headers)
    }

    /// Fetch usage data, returning cached value if still fresh.
    async fn fetch_usage(&self) -> anyhow::Result<(u64, u64, u8)> {
        {
            let cached = self.cached_usage.lock().await;
            if let Some((fetched_at, ref data)) = *cached {
                if fetched_at.elapsed().as_secs() < USAGE_CACHE_TTL_SECS {
                    return Ok((data.project_usage, data.project_cap, data.cap_reset_day));
                }
            }
        }

        let url = format!(
            "{X_API_BASE}/usage/tweets?usage.fields=project_usage,project_cap,cap_reset_day"
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.bearer_auth()?)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X usage API failed ({status}): {truncated}");
        }

        let body: serde_json::Value = resp.json().await?;
        let data = &body["data"];
        let project_usage = data["project_usage"].as_u64().unwrap_or(0);
        let project_cap = data["project_cap"].as_u64().unwrap_or(0);
        let cap_reset_day =
            u8::try_from(data["cap_reset_day"].as_u64().unwrap_or(1)).unwrap_or(1);

        let mut cached = self.cached_usage.lock().await;
        *cached = Some((
            Instant::now(),
            UsageData {
                project_usage,
                project_cap,
                cap_reset_day,
            },
        ));

        Ok((project_usage, project_cap, cap_reset_day))
    }

    /// Check usage limits before a write action. Returns an error message if over cap.
    async fn check_usage_limits(&self) -> Option<String> {
        if !self.check_before_action {
            return None;
        }

        let (project_usage, project_cap, _) = match self.fetch_usage().await {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("Failed to check X API usage before action: {e}");
                return None; // Don't block on usage check failures
            }
        };

        // Check local cap
        if let Some(local_cap) = self.monthly_tweet_cap {
            if project_usage >= local_cap {
                return Some(format!(
                    "X API monthly tweet cap reached: {project_usage}/{local_cap} tweets used \
                     (local limit). Increase x.usage.monthly_tweet_cap in config to allow more."
                ));
            }
        }

        // Check X project cap
        if project_cap > 0 && project_usage >= project_cap {
            return Some(format!(
                "X API project cap reached: {project_usage}/{project_cap} tweets used. \
                 Wait for cap reset or upgrade your plan."
            ));
        }

        // Warn if approaching cap
        let effective_cap = self.monthly_tweet_cap.unwrap_or(project_cap);
        if effective_cap > 0 {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let pct = (project_usage as f64 / effective_cap as f64 * 100.0).min(255.0) as u8;
            if pct >= self.warn_at_percent {
                tracing::warn!(
                    "X API usage at {pct}% ({project_usage}/{effective_cap} tweets). \
                     Warn threshold: {}%",
                    self.warn_at_percent
                );
            }
        }

        None
    }

    /// Resolve the user_id to use — from args if provided, otherwise from config.
    fn resolve_user_id<'a>(&'a self, args: &'a serde_json::Value) -> Option<&'a str> {
        args.get("user_id")
            .and_then(|v| v.as_str())
            .or_else(|| {
                if self.user_id.is_empty() {
                    None
                } else {
                    Some(self.user_id.as_str())
                }
            })
    }

    /// GET /2/users/{id}/tweets
    async fn list_tweets(
        &self,
        user_id: &str,
        max_results: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!(
            "{X_API_BASE}/users/{user_id}/tweets?max_results={max_results}\
             &tweet.fields=created_at,public_metrics,author_id,conversation_id"
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.bearer_auth()?)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X list_tweets failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// GET /2/users/{id}/mentions
    async fn get_mentions(
        &self,
        user_id: &str,
        max_results: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!(
            "{X_API_BASE}/users/{user_id}/mentions?max_results={max_results}\
             &tweet.fields=created_at,public_metrics,author_id,conversation_id"
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.bearer_auth()?)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X get_mentions failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// GET /2/tweets/search/recent
    async fn search_tweets(
        &self,
        query: &str,
        max_results: u64,
        sort_order: Option<&str>,
        start_time: Option<&str>,
        end_time: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut url = format!(
            "{X_API_BASE}/tweets/search/recent?query={}&max_results={max_results}\
             &tweet.fields=created_at,public_metrics,author_id,conversation_id\
             &expansions=author_id&user.fields=username,name",
            urlencoding::encode(query),
        );
        if let Some(order) = sort_order {
            let _ = write!(url, "&sort_order={order}");
        }
        if let Some(start) = start_time {
            let _ = write!(url, "&start_time={}", urlencoding::encode(start));
        }
        if let Some(end) = end_time {
            let _ = write!(url, "&end_time={}", urlencoding::encode(end));
        }
        let resp = self
            .http
            .get(&url)
            .headers(self.bearer_auth()?)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X search_tweets failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// POST /2/tweets
    async fn create_post(&self, text: &str) -> anyhow::Result<serde_json::Value> {
        let body = json!({ "text": text });
        let resp = self
            .http
            .post(format!("{X_API_BASE}/tweets"))
            .headers(self.oauth_auth()?)
            .json(&body)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X create_post failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// POST /2/tweets (with reply)
    async fn reply_to_post(
        &self,
        tweet_id: &str,
        text: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let body = json!({
            "text": text,
            "reply": {
                "in_reply_to_tweet_id": tweet_id
            }
        });
        let resp = self
            .http
            .post(format!("{X_API_BASE}/tweets"))
            .headers(self.oauth_auth()?)
            .json(&body)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X reply_to_post failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// POST /2/users/{id}/following
    async fn follow_user(&self, target_user_id: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{X_API_BASE}/users/{}/following", self.user_id);
        let body = json!({ "target_user_id": target_user_id });
        let resp = self
            .http
            .post(&url)
            .headers(self.oauth_auth()?)
            .json(&body)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X follow_user failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// DELETE /2/users/{source}/following/{target}
    async fn unfollow_user(&self, target_user_id: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!(
            "{X_API_BASE}/users/{}/following/{target_user_id}",
            self.user_id
        );
        let resp = self
            .http
            .delete(&url)
            .headers(self.oauth_auth()?)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X unfollow_user failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }

    /// GET /2/usage/tweets
    async fn get_usage(&self, days: u64) -> anyhow::Result<serde_json::Value> {
        let url = format!(
            "{X_API_BASE}/usage/tweets?days={days}\
             &usage.fields=project_usage,project_cap,cap_reset_day,daily_project_usage"
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.bearer_auth()?)
            .timeout(std::time::Duration::from_secs(X_REQUEST_TIMEOUT_SECS))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated = crate::util::truncate_with_ellipsis(&text, MAX_ERROR_BODY_CHARS);
            anyhow::bail!("X get_usage failed ({status}): {truncated}");
        }
        resp.json().await.map_err(Into::into)
    }
}

#[async_trait]
impl Tool for XTool {
    fn name(&self) -> &str {
        "x"
    }

    fn description(&self) -> &str {
        "Interact with X (Twitter): list tweets, search posts, create/reply to posts, follow/unfollow users, and monitor API usage."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "list_tweets", "get_mentions", "search_tweets",
                        "create_post", "reply_to_post",
                        "follow_user", "unfollow_user",
                        "get_usage"
                    ],
                    "description": "The X API action to perform"
                },
                "user_id": {
                    "type": "string",
                    "description": "X user ID (numeric). Defaults to configured user_id if omitted."
                },
                "query": {
                    "type": "string",
                    "description": "Search query for search_tweets (supports X search operators)"
                },
                "text": {
                    "type": "string",
                    "description": "Post text content (for create_post and reply_to_post)"
                },
                "tweet_id": {
                    "type": "string",
                    "description": "Tweet ID to reply to (for reply_to_post)"
                },
                "target_user_id": {
                    "type": "string",
                    "description": "User ID to follow or unfollow"
                },
                "sort_order": {
                    "type": "string",
                    "enum": ["recency", "relevancy"],
                    "description": "Sort order for search results"
                },
                "start_time": {
                    "type": "string",
                    "description": "Oldest date for search results (ISO 8601, e.g. 2026-03-20T00:00:00Z)"
                },
                "end_time": {
                    "type": "string",
                    "description": "Newest date for search results (ISO 8601, e.g. 2026-03-23T23:59:59Z)"
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 5,
                    "maximum": 100,
                    "description": "Maximum number of results (default: 10)"
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 90,
                    "description": "Number of days of usage history for get_usage (default: 7)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required parameter: action".into()),
                });
            }
        };

        // Check action is valid
        let operation = match action {
            "list_tweets" | "get_mentions" | "search_tweets" | "get_usage" => {
                ToolOperation::Read
            }
            "create_post" | "reply_to_post" | "follow_user" | "unfollow_user" => {
                ToolOperation::Act
            }
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Unknown action: {action}. Valid actions: list_tweets, get_mentions, \
                         search_tweets, create_post, reply_to_post, follow_user, unfollow_user, \
                         get_usage"
                    )),
                });
            }
        };

        // Check allowed_actions
        if !self.is_action_allowed(action) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Action '{action}' is not enabled. Add it to x.allowed_actions in config.toml. \
                     Currently allowed: {}",
                    self.allowed_actions.join(", ")
                )),
            });
        }

        // Enforce security policy
        if let Err(error) = self.security.enforce_tool_operation(operation, "x") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Check usage limits before write actions
        if matches!(
            action,
            "create_post" | "reply_to_post" | "follow_user" | "unfollow_user"
        ) {
            if let Some(msg) = self.check_usage_limits().await {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(msg),
                });
            }
        }

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .clamp(5, 100);

        let result = match action {
            "list_tweets" => {
                let user_id = match self.resolve_user_id(&args) {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "list_tweets requires user_id parameter or x.user_id in config"
                                    .into(),
                            ),
                        });
                    }
                };
                self.list_tweets(user_id, max_results).await
            }
            "get_mentions" => {
                let user_id = match self.resolve_user_id(&args) {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "get_mentions requires user_id parameter or x.user_id in config"
                                    .into(),
                            ),
                        });
                    }
                };
                self.get_mentions(user_id, max_results).await
            }
            "search_tweets" => {
                let query = match args.get("query").and_then(|v| v.as_str()) {
                    Some(q) => q,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("search_tweets requires query parameter".into()),
                        });
                    }
                };
                let sort_order = args.get("sort_order").and_then(|v| v.as_str());
                let start_time = args.get("start_time").and_then(|v| v.as_str());
                let end_time = args.get("end_time").and_then(|v| v.as_str());
                self.search_tweets(query, max_results, sort_order, start_time, end_time)
                    .await
            }
            "create_post" => {
                let text = match args.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("create_post requires text parameter".into()),
                        });
                    }
                };
                self.create_post(text).await
            }
            "reply_to_post" => {
                let tweet_id = match args.get("tweet_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("reply_to_post requires tweet_id parameter".into()),
                        });
                    }
                };
                let text = match args.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("reply_to_post requires text parameter".into()),
                        });
                    }
                };
                self.reply_to_post(tweet_id, text).await
            }
            "follow_user" => {
                if self.user_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "follow_user requires x.user_id in config (source user)".into(),
                        ),
                    });
                }
                let target = match args.get("target_user_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("follow_user requires target_user_id parameter".into()),
                        });
                    }
                };
                self.follow_user(target).await
            }
            "unfollow_user" => {
                if self.user_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "unfollow_user requires x.user_id in config (source user)".into(),
                        ),
                    });
                }
                let target = match args.get("target_user_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("unfollow_user requires target_user_id parameter".into()),
                        });
                    }
                };
                self.unfollow_user(target).await
            }
            "get_usage" => {
                let days = args
                    .get("days")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(7)
                    .clamp(1, 90);
                self.get_usage(days).await
            }
            _ => unreachable!(), // Already handled above
        };

        match result {
            Ok(value) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| value.to_string()),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;

    fn test_tool() -> XTool {
        let security = Arc::new(SecurityPolicy::default());
        XTool::new(
            "test-bearer".into(),
            "test-access".into(),
            "123456".into(),
            vec![
                "list_tweets".into(),
                "get_mentions".into(),
                "search_tweets".into(),
                "create_post".into(),
                "reply_to_post".into(),
                "follow_user".into(),
                "unfollow_user".into(),
                "get_usage".into(),
            ],
            security,
            None,
            80,
            false,
        )
    }

    fn read_only_tool() -> XTool {
        let security = Arc::new(SecurityPolicy::default());
        XTool::new(
            "test-bearer".into(),
            "test-access".into(),
            "123456".into(),
            vec![
                "list_tweets".into(),
                "get_mentions".into(),
                "search_tweets".into(),
            ],
            security,
            None,
            80,
            false,
        )
    }

    #[test]
    fn tool_name_is_x() {
        let tool = test_tool();
        assert_eq!(tool.name(), "x");
    }

    #[test]
    fn parameters_schema_has_required_action() {
        let tool = test_tool();
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("action")));
    }

    #[test]
    fn parameters_schema_defines_all_actions() {
        let tool = test_tool();
        let schema = tool.parameters_schema();
        let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
        let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
        assert!(action_strs.contains(&"list_tweets"));
        assert!(action_strs.contains(&"get_mentions"));
        assert!(action_strs.contains(&"search_tweets"));
        assert!(action_strs.contains(&"create_post"));
        assert!(action_strs.contains(&"reply_to_post"));
        assert!(action_strs.contains(&"follow_user"));
        assert!(action_strs.contains(&"unfollow_user"));
        assert!(action_strs.contains(&"get_usage"));
    }

    #[tokio::test]
    async fn execute_missing_action_returns_error() {
        let tool = test_tool();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("action"));
    }

    #[tokio::test]
    async fn execute_unknown_action_returns_error() {
        let tool = test_tool();
        let result = tool.execute(json!({"action": "invalid"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn execute_disallowed_action_returns_error() {
        let tool = read_only_tool();
        let result = tool
            .execute(json!({"action": "create_post", "text": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("not enabled"));
    }

    #[tokio::test]
    async fn execute_list_tweets_missing_user_id_returns_error() {
        let security = Arc::new(SecurityPolicy::default());
        let tool = XTool::new(
            "test-bearer".into(),
            "test-access".into(),
            String::new(), // no default user_id
            vec!["list_tweets".into()],
            security,
            None,
            80,
            false,
        );
        let result = tool
            .execute(json!({"action": "list_tweets"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("user_id"));
    }

    #[tokio::test]
    async fn execute_search_tweets_missing_query_returns_error() {
        let tool = test_tool();
        let result = tool
            .execute(json!({"action": "search_tweets"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("query"));
    }

    #[tokio::test]
    async fn execute_create_post_missing_text_returns_error() {
        let tool = test_tool();
        let result = tool
            .execute(json!({"action": "create_post"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("text"));
    }

    #[tokio::test]
    async fn execute_reply_to_post_missing_tweet_id_returns_error() {
        let tool = test_tool();
        let result = tool
            .execute(json!({"action": "reply_to_post", "text": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("tweet_id"));
    }

    #[tokio::test]
    async fn execute_reply_to_post_missing_text_returns_error() {
        let tool = test_tool();
        let result = tool
            .execute(json!({"action": "reply_to_post", "tweet_id": "123"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("text"));
    }

    #[tokio::test]
    async fn execute_follow_user_missing_target_returns_error() {
        let tool = test_tool();
        let result = tool
            .execute(json!({"action": "follow_user"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("target_user_id"));
    }

    #[tokio::test]
    async fn execute_unfollow_user_missing_target_returns_error() {
        let tool = test_tool();
        let result = tool
            .execute(json!({"action": "unfollow_user"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("target_user_id"));
    }

    #[tokio::test]
    async fn execute_follow_user_missing_config_user_id_returns_error() {
        let security = Arc::new(SecurityPolicy::default());
        let tool = XTool::new(
            "test-bearer".into(),
            "test-access".into(),
            String::new(), // no configured user_id
            vec!["follow_user".into()],
            security,
            None,
            80,
            false,
        );
        let result = tool
            .execute(json!({"action": "follow_user", "target_user_id": "789"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("x.user_id"));
    }
}
