use super::traits::{Tool, ToolResult};
use crate::channels::agentphone::OutboundSessionTracker;
use crate::config::PhoneNumberEntry;
use crate::memory::{Memory, MemoryCategory};
use crate::security::{policy::ToolOperation, SecurityPolicy};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

const API_BASE: &str = "https://api.agentphone.to/v1";
const REQUEST_TIMEOUT_SECS: u64 = 30;
const MAX_ERROR_BODY_CHARS: usize = 500;

/// Tool for interacting with the AgentPhone API — send SMS, place calls,
/// manage agents/numbers, query conversations and usage.
///
/// Each action is gated by `allowed_actions` and the appropriate security
/// operation (Read for queries, Act for mutations). Outbound SMS and calls
/// are additionally restricted to `allowed_numbers`.
pub struct AgentPhoneTool {
    api_key: String,
    default_agent_id: Option<String>,
    default_from_number_id: Option<String>,
    allowed_numbers: Vec<PhoneNumberEntry>,
    allowed_actions: Vec<String>,
    default_voice: Option<String>,
    default_begin_message: Option<String>,
    http: reqwest::Client,
    security: Arc<SecurityPolicy>,
    mem: Arc<dyn Memory>,
    outbound_tracker: Option<OutboundSessionTracker>,
}

impl AgentPhoneTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_key: String,
        default_agent_id: Option<String>,
        default_from_number_id: Option<String>,
        allowed_numbers: Vec<PhoneNumberEntry>,
        allowed_actions: Vec<String>,
        default_voice: Option<String>,
        default_begin_message: Option<String>,
        security: Arc<SecurityPolicy>,
        mem: Arc<dyn Memory>,
    ) -> Self {
        Self {
            api_key,
            default_agent_id,
            default_from_number_id,
            allowed_numbers,
            allowed_actions,
            default_voice,
            default_begin_message,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build()
                .unwrap_or_default(),
            security,
            mem,
            outbound_tracker: None,
        }
    }

    /// Set the shared outbound session tracker.
    pub fn with_outbound_tracker(mut self, tracker: OutboundSessionTracker) -> Self {
        self.outbound_tracker = Some(tracker);
        self
    }

    fn is_action_allowed(&self, action: &str) -> bool {
        self.allowed_actions.iter().any(|a| a == action)
    }

    fn is_number_allowed(&self, phone: &str) -> bool {
        self.allowed_numbers
            .iter()
            .any(|entry| entry.number() == "*" || entry.number() == phone)
    }

    /// Get the configured purpose for a number, if any.
    fn get_number_purpose(&self, phone: &str) -> Option<&str> {
        self.allowed_numbers.iter().find_map(|entry| {
            if entry.number() == "*" || entry.number() == phone {
                entry.purpose()
            } else {
                None
            }
        })
    }

    /// Register an outbound session on the shared tracker (if available).
    fn register_outbound_session(&self, phone: &str, call_id: &str, purpose: Option<&str>, is_voice: bool) {
        if let Some(ref tracker) = self.outbound_tracker {
            let ttl = if is_voice { 3600u64 } else { 1800u64 };
            let session = crate::channels::agentphone::OutboundSession {
                call_id: call_id.to_string(),
                started_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                purpose: purpose.map(|s| s.to_string()),
                ttl_secs: ttl,
            };
            let mut sessions = tracker.lock().unwrap();
            sessions.insert(phone.to_string(), session);
        }
    }

    async fn api_get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{API_BASE}{path}");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn api_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{API_BASE}{path}");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn api_patch(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{API_BASE}{path}");
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn handle_response(
        &self,
        resp: reqwest::Response,
    ) -> anyhow::Result<serde_json::Value> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let truncated = if body.len() > MAX_ERROR_BODY_CHARS {
                format!("{}…", &body[..MAX_ERROR_BODY_CHARS])
            } else {
                body
            };
            anyhow::bail!("AgentPhone API error {status}: {truncated}");
        }
        let body = resp.text().await.unwrap_or_default();
        if body.is_empty() {
            Ok(json!({"status": "ok"}))
        } else {
            Ok(serde_json::from_str(&body)?)
        }
    }

    // ── Action handlers ──

    async fn agents_list(&self, args: &serde_json::Value) -> ToolResult {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20);
        match self.api_get(&format!("/agents?limit={limit}")).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn agents_get(&self, args: &serde_json::Value) -> ToolResult {
        let Some(agent_id) = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .or(self.default_agent_id.as_deref())
        else {
            return ToolResult::error("agents.get requires agent_id parameter".into());
        };
        match self.api_get(&format!("/agents/{agent_id}")).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn agents_create(&self, args: &serde_json::Value) -> ToolResult {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolResult::error("agents.create requires name parameter".into());
        };
        let mut body = json!({"name": name});
        if let Some(desc) = args.get("description").and_then(|v| v.as_str()) {
            body["description"] = json!(desc);
        }
        if let Some(voice) = args
            .get("voice")
            .and_then(|v| v.as_str())
            .or(self.default_voice.as_deref())
        {
            body["voice"] = json!(voice);
        }
        if let Some(begin_message) = args
            .get("begin_message")
            .and_then(|v| v.as_str())
            .or(self.default_begin_message.as_deref())
        {
            body["beginMessage"] = json!(begin_message);
        }
        if let Some(voice_mode) = args.get("voice_mode").and_then(|v| v.as_str()) {
            body["voiceMode"] = json!(voice_mode);
        }
        match self.api_post("/agents", &body).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn agents_update(&self, args: &serde_json::Value) -> ToolResult {
        let Some(agent_id) = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .or(self.default_agent_id.as_deref())
        else {
            return ToolResult::error("agents.update requires agent_id parameter".into());
        };
        let mut body = json!({});
        if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
            body["name"] = json!(name);
        }
        if let Some(desc) = args.get("description").and_then(|v| v.as_str()) {
            body["description"] = json!(desc);
        }
        if let Some(voice) = args.get("voice").and_then(|v| v.as_str()) {
            body["voice"] = json!(voice);
        }
        if let Some(begin_message) = args.get("begin_message").and_then(|v| v.as_str()) {
            body["beginMessage"] = json!(begin_message);
        }
        if let Some(voice_mode) = args.get("voice_mode").and_then(|v| v.as_str()) {
            body["voiceMode"] = json!(voice_mode);
        }
        if let Some(system_prompt) = args.get("system_prompt").and_then(|v| v.as_str()) {
            body["systemPrompt"] = json!(system_prompt);
        }
        match self.api_patch(&format!("/agents/{agent_id}"), &body).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn numbers_list(&self, args: &serde_json::Value) -> ToolResult {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20);
        match self.api_get(&format!("/numbers?limit={limit}")).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn calls_list(&self, args: &serde_json::Value) -> ToolResult {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20);
        match self.api_get(&format!("/calls?limit={limit}")).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn calls_get(&self, args: &serde_json::Value) -> ToolResult {
        let Some(call_id) = args.get("call_id").and_then(|v| v.as_str()) else {
            return ToolResult::error("calls.get requires call_id parameter".into());
        };
        match self.api_get(&format!("/calls/{call_id}")).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn calls_create(&self, args: &serde_json::Value) -> ToolResult {
        let Some(to_number) = args.get("to").and_then(|v| v.as_str()) else {
            return ToolResult::error("calls.create requires 'to' parameter (E.164 phone number)".into());
        };

        // Enforce allowed_numbers restriction
        if !self.is_number_allowed(to_number) {
            return ToolResult::error(format!(
                "Phone number {to_number} is not in the allowed_numbers list. \
                Add it to [agentphone].allowed_numbers in config.toml."
            ));
        }

        let agent_id = match args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .or(self.default_agent_id.as_deref())
        {
            Some(id) => id,
            None => {
                return ToolResult::error("calls.create requires agent_id (or configure default_agent_id)".into());
            }
        };

        let mut body = json!({
            "agentId": agent_id,
            "toNumber": to_number,
        });

        if let Some(from_id) = args
            .get("from_number_id")
            .and_then(|v| v.as_str())
            .or(self.default_from_number_id.as_deref())
        {
            body["fromNumberId"] = json!(from_id);
        }

        if let Some(voice) = args
            .get("voice")
            .and_then(|v| v.as_str())
            .or(self.default_voice.as_deref())
        {
            body["voice"] = json!(voice);
        }

        if let Some(greeting) = args
            .get("initial_greeting")
            .and_then(|v| v.as_str())
            .or(self.default_begin_message.as_deref())
        {
            body["initialGreeting"] = json!(greeting);
        }

        if let Some(prompt) = args.get("system_prompt").and_then(|v| v.as_str()) {
            body["systemPrompt"] = json!(prompt);
        }

        // Resolve purpose: explicit arg > config default > none
        let purpose = args
            .get("purpose")
            .and_then(|v| v.as_str())
            .or_else(|| self.get_number_purpose(to_number));

        match self.api_post("/calls", &body).await {
            Ok(data) => {
                // Register outbound session so inbound webhooks from this number are accepted
                let call_id = data.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                self.register_outbound_session(to_number, call_id, purpose, true);

                // Store outbound call context in memory for counterparty session
                let session_id = format!("agentphone_voice_{to_number}");
                let greeting = body.get("initialGreeting").and_then(|v| v.as_str()).unwrap_or("default");
                let prompt_val = body.get("systemPrompt").and_then(|v| v.as_str()).unwrap_or("none");
                let purpose_str = purpose.unwrap_or("none");
                let context = format!(
                    "[outbound call placed] to: {to_number}, greeting: {greeting}, prompt: {prompt_val}, purpose: {purpose_str}"
                );
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let key = format!("agentphone_out_{to_number}_{now}");
                let _ = self.mem.store(&key, &context, MemoryCategory::Conversation, Some(&session_id)).await;

                ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default())
            }
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn sms_send(&self, args: &serde_json::Value) -> ToolResult {
        let Some(to_number) = args.get("to").and_then(|v| v.as_str()) else {
            return ToolResult::error("sms.send requires 'to' parameter (E.164 phone number)".into());
        };

        // Enforce allowed_numbers restriction
        if !self.is_number_allowed(to_number) {
            return ToolResult::error(format!(
                "Phone number {to_number} is not in the allowed_numbers list. \
                Add it to [agentphone].allowed_numbers in config.toml."
            ));
        }

        let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
            return ToolResult::error("sms.send requires 'message' parameter".into());
        };

        let agent_id = match args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .or(self.default_agent_id.as_deref())
        {
            Some(id) => id,
            None => {
                return ToolResult::error("sms.send requires agent_id (or configure default_agent_id)".into());
            }
        };

        // Find existing conversation with this number via the agent's conversations
        let convs = match self
            .api_get(&format!("/agents/{agent_id}/conversations?limit=100"))
            .await
        {
            Ok(data) => data,
            Err(e) => return ToolResult::error(format!("Failed to list conversations: {e:#}")),
        };

        let conversation_id = convs.as_array().and_then(|arr| {
            arr.iter().find_map(|c| {
                let participant = c.get("participant").and_then(|v| v.as_str())?;
                if participant == to_number
                    || participant == to_number.strip_prefix('+').unwrap_or("")
                {
                    c.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
        });

        // Resolve purpose: explicit arg > config default > none
        let purpose = args
            .get("purpose")
            .and_then(|v| v.as_str())
            .or_else(|| self.get_number_purpose(to_number));

        if let Some(conv_id) = conversation_id {
            // Update conversation metadata to trigger reply
            let body = json!({"metadata": {"_reply": message}});
            match self.api_patch(&format!("/conversations/{conv_id}"), &body).await {
                Ok(data) => {
                    // Register outbound session so inbound replies are accepted
                    self.register_outbound_session(to_number, &conv_id, purpose, false);

                    // Store outbound SMS context in memory for counterparty session
                    let session_id = format!("agentphone_sms_{to_number}");
                    let purpose_str = purpose.unwrap_or("none");
                    let context = format!("[outbound SMS sent] to: {to_number}, message: {message}, purpose: {purpose_str}");
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let key = format!("agentphone_out_{to_number}_{now}");
                    let _ = self.mem.store(&key, &context, MemoryCategory::Conversation, Some(&session_id)).await;

                    ToolResult::success(
                        serde_json::to_string_pretty(&data).unwrap_or_default(),
                    )
                }
                Err(e) => ToolResult::error(format!("{e:#}")),
            }
        } else {
            ToolResult::error(format!(
                "No existing conversation found with {to_number}. \
                An inbound message is required before sending SMS."
            ))
        }
    }

    async fn conversations_list(&self, args: &serde_json::Value) -> ToolResult {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20);
        match self.api_get(&format!("/conversations?limit={limit}")).await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn conversations_get(&self, args: &serde_json::Value) -> ToolResult {
        let Some(conv_id) = args.get("conversation_id").and_then(|v| v.as_str()) else {
            return ToolResult::error("conversations.get requires conversation_id parameter".into());
        };
        let message_limit = args
            .get("message_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50);
        match self
            .api_get(&format!(
                "/conversations/{conv_id}?message_limit={message_limit}"
            ))
            .await
        {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn conversations_update(&self, args: &serde_json::Value) -> ToolResult {
        let Some(conv_id) = args.get("conversation_id").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "conversations.update requires conversation_id parameter".into(),
            );
        };
        let Some(metadata) = args.get("metadata") else {
            return ToolResult::error("conversations.update requires metadata parameter".into());
        };
        let body = json!({"metadata": metadata});
        match self
            .api_patch(&format!("/conversations/{conv_id}"), &body)
            .await
        {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }

    async fn usage_get(&self) -> ToolResult {
        match self.api_get("/usage").await {
            Ok(data) => ToolResult::success(serde_json::to_string_pretty(&data).unwrap_or_default()),
            Err(e) => ToolResult::error(format!("{e:#}")),
        }
    }
}

impl ToolResult {
    fn success(output: String) -> Self {
        Self {
            success: true,
            output,
            error: None,
        }
    }

    fn error(msg: String) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(msg),
        }
    }
}

#[async_trait]
impl Tool for AgentPhoneTool {
    fn name(&self) -> &str {
        "agentphone"
    }

    fn description(&self) -> &str {
        "Interact with AgentPhone: send SMS, place/list calls, manage agents and phone numbers, \
        view conversations, and monitor usage."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "agents.list", "agents.get", "agents.create", "agents.update",
                        "numbers.list",
                        "calls.list", "calls.get", "calls.create",
                        "sms.send",
                        "conversations.list", "conversations.get", "conversations.update",
                        "usage.get"
                    ],
                    "description": "The AgentPhone API action to perform"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Agent ID. Defaults to configured default_agent_id if omitted."
                },
                "call_id": {
                    "type": "string",
                    "description": "Call ID (for calls.get)"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Conversation ID (for conversations.get/update)"
                },
                "to": {
                    "type": "string",
                    "description": "Destination phone number in E.164 format (for sms.send, calls.create)"
                },
                "message": {
                    "type": "string",
                    "description": "SMS message text (for sms.send)"
                },
                "from_number_id": {
                    "type": "string",
                    "description": "Phone number ID to call/send from (for calls.create, sms.send)"
                },
                "name": {
                    "type": "string",
                    "description": "Agent name (for agents.create/update)"
                },
                "description": {
                    "type": "string",
                    "description": "Agent description (for agents.create/update)"
                },
                "voice": {
                    "type": "string",
                    "description": "TTS voice (e.g. \"Polly.Amy\") for calls.create or agents.create/update"
                },
                "voice_mode": {
                    "type": "string",
                    "enum": ["webhook", "hosted"],
                    "description": "Voice mode for agents.create/update"
                },
                "initial_greeting": {
                    "type": "string",
                    "description": "Greeting message for outbound calls (calls.create)"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "System prompt for hosted voice mode (agents.update, calls.create)"
                },
                "begin_message": {
                    "type": "string",
                    "description": "Begin message for inbound calls (agents.create/update)"
                },
                "purpose": {
                    "type": "string",
                    "description": "Purpose of this outbound contact, e.g. 'schedule dentist appointment'. \
                    Used to scope information disclosure during the conversation. (for calls.create, sms.send)"
                },
                "metadata": {
                    "type": "object",
                    "description": "Metadata object (for conversations.update)"
                },
                "message_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Number of messages to return (for conversations.get, default: 50)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Number of results to return (default: 20)"
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

        // Map action to security operation
        let operation = match action {
            "agents.list" | "agents.get" | "numbers.list" | "calls.list" | "calls.get"
            | "conversations.list" | "conversations.get" | "usage.get" => ToolOperation::Read,
            "sms.send" | "calls.create" | "agents.create" | "agents.update"
            | "conversations.update" => ToolOperation::Act,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Unknown action: {action}. Valid actions: agents.list, agents.get, \
                        agents.create, agents.update, numbers.list, calls.list, calls.get, \
                        calls.create, sms.send, conversations.list, conversations.get, \
                        conversations.update, usage.get"
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
                    "Action '{action}' is not enabled. Add it to [agentphone].allowed_actions \
                    in config.toml. Currently allowed: {}",
                    self.allowed_actions.join(", ")
                )),
            });
        }

        // Enforce security policy
        if let Err(error) = self
            .security
            .enforce_tool_operation(operation, "agentphone")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Dispatch to action handler
        let result = match action {
            "agents.list" => self.agents_list(&args).await,
            "agents.get" => self.agents_get(&args).await,
            "agents.create" => self.agents_create(&args).await,
            "agents.update" => self.agents_update(&args).await,
            "numbers.list" => self.numbers_list(&args).await,
            "calls.list" => self.calls_list(&args).await,
            "calls.get" => self.calls_get(&args).await,
            "calls.create" => self.calls_create(&args).await,
            "sms.send" => self.sms_send(&args).await,
            "conversations.list" => self.conversations_list(&args).await,
            "conversations.get" => self.conversations_get(&args).await,
            "conversations.update" => self.conversations_update(&args).await,
            "usage.get" => self.usage_get().await,
            _ => unreachable!(),
        };

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AutonomyConfig, PhoneNumberConfig, PhoneTrustLevel};
    use crate::memory::none::NoneMemory;
    use crate::security::SecurityPolicy;
    use std::path::PathBuf;

    fn make_tool() -> AgentPhoneTool {
        let workspace = PathBuf::from("/tmp/test");
        let security = Arc::new(SecurityPolicy::from_config(
            &AutonomyConfig::default(),
            &workspace,
        ));
        let mem: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        AgentPhoneTool::new(
            "test-api-key".into(),
            Some("agt_test".into()),
            Some("num_test".into()),
            vec![PhoneNumberEntry::Simple("+15551234567".into())],
            vec![
                "agents.list".into(),
                "calls.create".into(),
                "sms.send".into(),
            ],
            Some("Polly.Amy".into()),
            Some("Hello!".into()),
            security,
            mem,
        )
    }

    #[test]
    fn tool_name() {
        assert_eq!(make_tool().name(), "agentphone");
    }

    #[test]
    fn action_allowed_check() {
        let tool = make_tool();
        assert!(tool.is_action_allowed("agents.list"));
        assert!(tool.is_action_allowed("calls.create"));
        assert!(!tool.is_action_allowed("agents.create"));
    }

    #[test]
    fn number_allowed_check() {
        let tool = make_tool();
        assert!(tool.is_number_allowed("+15551234567"));
        assert!(!tool.is_number_allowed("+15559999999"));
    }

    #[test]
    fn number_allowed_wildcard() {
        let workspace = PathBuf::from("/tmp/test");
        let security = Arc::new(SecurityPolicy::from_config(
            &AutonomyConfig::default(),
            &workspace,
        ));
        let mem: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let tool = AgentPhoneTool::new(
            "key".into(),
            None,
            None,
            vec![PhoneNumberEntry::Simple("*".into())],
            vec![],
            None,
            None,
            security,
            mem,
        );
        assert!(tool.is_number_allowed("+15559999999"));
    }

    #[test]
    fn number_allowed_empty_denies_all() {
        let workspace = PathBuf::from("/tmp/test");
        let security = Arc::new(SecurityPolicy::from_config(
            &AutonomyConfig::default(),
            &workspace,
        ));
        let mem: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let tool = AgentPhoneTool::new(
            "key".into(),
            None,
            None,
            vec![],
            vec![],
            None,
            None,
            security,
            mem,
        );
        assert!(!tool.is_number_allowed("+15551234567"));
    }

    #[test]
    fn number_purpose_lookup() {
        let workspace = PathBuf::from("/tmp/test");
        let security = Arc::new(SecurityPolicy::from_config(
            &AutonomyConfig::default(),
            &workspace,
        ));
        let mem: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let tool = AgentPhoneTool::new(
            "key".into(),
            None,
            None,
            vec![
                PhoneNumberEntry::Simple("+15551234567".into()),
                PhoneNumberEntry::Detailed(PhoneNumberConfig {
                    number: "+15559876543".into(),
                    trust: PhoneTrustLevel::Scoped,
                    purpose: Some("dentist appointment".into()),
                }),
            ],
            vec![],
            None,
            None,
            security,
            mem,
        );
        assert_eq!(tool.get_number_purpose("+15551234567"), None);
        assert_eq!(tool.get_number_purpose("+15559876543"), Some("dentist appointment"));
    }

    #[tokio::test]
    async fn execute_missing_action() {
        let tool = make_tool();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Missing required parameter"));
    }

    #[tokio::test]
    async fn execute_unknown_action() {
        let tool = make_tool();
        let result = tool.execute(json!({"action": "foo.bar"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn execute_disallowed_action() {
        let tool = make_tool();
        let result = tool
            .execute(json!({"action": "agents.create", "name": "Test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not enabled"));
    }
}
