use super::traits::{Channel, ChannelMessage, SendMessage};
use crate::config::{PhoneNumberEntry, PhoneTrustLevel};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const API_BASE: &str = "https://api.agentphone.to/v1";

/// Default TTL for voice outbound sessions (1 hour).
const VOICE_SESSION_TTL_SECS: u64 = 3600;
/// Default TTL for SMS outbound sessions (30 minutes).
const SMS_SESSION_TTL_SECS: u64 = 1800;

/// Tracks an active outbound call or SMS session so that inbound webhooks
/// from the counterparty are accepted even if the number is not in the
/// channel's allowlist.
#[derive(Debug, Clone)]
pub struct OutboundSession {
    /// Call or conversation ID from the AgentPhone API.
    pub call_id: String,
    /// Unix timestamp when the session was registered.
    pub started_at: u64,
    /// Purpose of this outbound contact (used for information scoping).
    pub purpose: Option<String>,
    /// Time-to-live in seconds. Session is expired after `started_at + ttl_secs`.
    pub ttl_secs: u64,
}

impl OutboundSession {
    fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now > self.started_at + self.ttl_secs
    }
}

/// Shared tracker for active outbound sessions. Used by both `AgentPhoneChannel`
/// and `AgentPhoneTool` to coordinate which numbers have active outbound sessions.
pub type OutboundSessionTracker = Arc<Mutex<HashMap<String, OutboundSession>>>;

/// Create a new shared outbound session tracker.
pub fn new_outbound_tracker() -> OutboundSessionTracker {
    Arc::new(Mutex::new(HashMap::new()))
}

/// AgentPhone channel — receives inbound SMS and voice call transcripts via webhooks.
///
/// This channel operates in webhook mode (push-based). Messages are received via the
/// gateway's `/agentphone` webhook endpoint. The `listen` method is a no-op; actual
/// message handling happens when AgentPhone POSTs webhook events.
///
/// Outbound SMS replies are sent via the AgentPhone API.
pub struct AgentPhoneChannel {
    api_key: String,
    webhook_secret: String,
    agent_id: Option<String>,
    agent_phone_number: Option<String>,
    allowed_numbers: Vec<PhoneNumberEntry>,
    voice: Option<String>,
    begin_message: Option<String>,
    conversation_prompt: String,
    model: Option<String>,
    proxy_url: Option<String>,
    http: reqwest::Client,
    active_outbound: OutboundSessionTracker,
}

impl AgentPhoneChannel {
    pub fn new(
        api_key: String,
        webhook_secret: String,
        allowed_numbers: Vec<PhoneNumberEntry>,
    ) -> Self {
        Self {
            api_key,
            webhook_secret,
            allowed_numbers,
            agent_id: None,
            agent_phone_number: None,
            voice: None,
            begin_message: None,
            conversation_prompt: String::new(),
            model: None,
            proxy_url: None,
            http: reqwest::Client::new(),
            active_outbound: new_outbound_tracker(),
        }
    }

    pub fn with_agent_id(mut self, agent_id: Option<String>) -> Self {
        self.agent_id = agent_id;
        self
    }

    pub fn with_agent_phone_number(mut self, number: Option<String>) -> Self {
        self.agent_phone_number = number;
        self
    }

    pub fn with_voice(mut self, voice: Option<String>) -> Self {
        self.voice = voice;
        self
    }

    pub fn with_begin_message(mut self, begin_message: Option<String>) -> Self {
        self.begin_message = begin_message;
        self
    }

    pub fn with_conversation_prompt(mut self, prompt: String) -> Self {
        self.conversation_prompt = prompt;
        self
    }

    /// Get the conversation prompt preamble for Request/Response interactions.
    pub fn conversation_prompt(&self) -> &str {
        &self.conversation_prompt
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Get the channel-specific model override, if configured.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn with_proxy_url(mut self, proxy_url: Option<String>) -> Self {
        if proxy_url.is_some() {
            self.http = crate::config::build_channel_proxy_client(
                "channel.agentphone",
                proxy_url.as_deref(),
            );
        }
        self.proxy_url = proxy_url;
        self
    }

    /// Get the webhook secret for signature verification.
    pub fn webhook_secret(&self) -> &str {
        &self.webhook_secret
    }

    /// Get the API key.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Set the shared outbound session tracker (called during gateway setup).
    pub fn with_outbound_tracker(mut self, tracker: OutboundSessionTracker) -> Self {
        self.active_outbound = tracker;
        self
    }

    /// Get a reference to the outbound session tracker.
    pub fn outbound_tracker(&self) -> &OutboundSessionTracker {
        &self.active_outbound
    }

    /// Check if a phone number is in the configured allowlist (E.164 format: +1234567890).
    pub fn is_number_allowed(&self, phone: &str) -> bool {
        self.allowed_numbers
            .iter()
            .any(|entry| entry.number() == "*" || entry.number() == phone)
    }

    /// Check if a phone number is trusted (i.e. in the allowlist with Trusted trust level).
    /// Returns false for scoped numbers and numbers not in the list.
    pub fn is_trusted_number(&self, phone: &str) -> bool {
        self.allowed_numbers.iter().any(|entry| {
            (entry.number() == "*" || entry.number() == phone)
                && entry.trust() == PhoneTrustLevel::Trusted
        })
    }

    /// Get the trust level for a phone number, if it's in the allowlist.
    pub fn get_trust_level(&self, phone: &str) -> Option<PhoneTrustLevel> {
        self.allowed_numbers.iter().find_map(|entry| {
            if entry.number() == "*" || entry.number() == phone {
                Some(entry.trust())
            } else {
                None
            }
        })
    }

    /// Get the configured purpose for a phone number, if any.
    pub fn get_number_purpose(&self, phone: &str) -> Option<&str> {
        self.allowed_numbers.iter().find_map(|entry| {
            if entry.number() == "*" || entry.number() == phone {
                entry.purpose()
            } else {
                None
            }
        })
    }

    /// Check if a phone number has an active (non-expired) outbound session.
    pub fn has_active_outbound(&self, phone: &str) -> bool {
        let sessions = self.active_outbound.lock().unwrap();
        sessions
            .get(phone)
            .map(|s| !s.is_expired())
            .unwrap_or(false)
    }

    /// Get the purpose from an active outbound session, if one exists.
    pub fn get_outbound_purpose(&self, phone: &str) -> Option<String> {
        let sessions = self.active_outbound.lock().unwrap();
        sessions
            .get(phone)
            .filter(|s| !s.is_expired())
            .and_then(|s| s.purpose.clone())
    }

    /// Register an active outbound session for a phone number.
    pub fn register_outbound(&self, phone: &str, call_id: &str, purpose: Option<&str>, is_voice: bool) {
        let ttl = if is_voice {
            VOICE_SESSION_TTL_SECS
        } else {
            SMS_SESSION_TTL_SECS
        };
        let session = OutboundSession {
            call_id: call_id.to_string(),
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            purpose: purpose.map(|s| s.to_string()),
            ttl_secs: ttl,
        };
        let mut sessions = self.active_outbound.lock().unwrap();
        sessions.insert(phone.to_string(), session);
    }

    /// Clear an active outbound session for a phone number.
    pub fn clear_outbound(&self, phone: &str) {
        let mut sessions = self.active_outbound.lock().unwrap();
        sessions.remove(phone);
    }

    /// Check whether an inbound webhook from this number should be accepted.
    pub fn should_accept_inbound(&self, phone: &str) -> bool {
        self.is_number_allowed(phone)
    }

    /// Sync voice and begin_message to the AgentPhone agent on startup.
    pub async fn sync_agent_config(&self) -> anyhow::Result<()> {
        let Some(ref agent_id) = self.agent_id else {
            return Ok(());
        };

        if self.voice.is_none() && self.begin_message.is_none() {
            return Ok(());
        }

        let mut body = serde_json::json!({
            "voiceMode": "webhook",
        });

        if let Some(ref voice) = self.voice {
            body["voice"] = serde_json::json!(voice);
        }
        if let Some(ref begin_message) = self.begin_message {
            body["beginMessage"] = serde_json::json!(begin_message);
        }

        let url = format!("{API_BASE}/agents/{agent_id}");
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            tracing::info!("AgentPhone: synced agent {agent_id} voice/greeting config");
        } else {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "AgentPhone: failed to sync agent config: {status} — {error_body}"
            );
        }

        Ok(())
    }

    /// Verify webhook signature using HMAC-SHA256.
    ///
    /// AgentPhone signs webhooks with `X-Webhook-Signature: sha256=<hex>` using
    /// `{timestamp}.{body}` as the signed payload.
    pub fn verify_signature(
        &self,
        body: &[u8],
        signature: Option<&str>,
        timestamp: Option<&str>,
    ) -> bool {
        if self.webhook_secret.is_empty() {
            return true; // No secret configured → accept all
        }

        let Some(sig_header) = signature else {
            return false; // Secret configured but no signature → reject
        };

        let Some(hex_sig) = sig_header.strip_prefix("sha256=") else {
            return false;
        };

        // Replay protection: reject timestamps with >300s drift
        if let Some(ts_str) = timestamp {
            if let Ok(ts) = ts_str.parse::<i64>() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                if (now - ts).unsigned_abs() > 300 {
                    tracing::warn!("AgentPhone: webhook timestamp drift too large ({ts}), rejecting");
                    return false;
                }
            }
        }

        let Ok(expected) = hex::decode(hex_sig) else {
            return false;
        };

        // Sign: {timestamp}.{body}
        let signed_payload = if let Some(ts) = timestamp {
            format!("{ts}.{}", String::from_utf8_lossy(body))
        } else {
            String::from_utf8_lossy(body).to_string()
        };

        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(self.webhook_secret.as_bytes()) else {
            return false;
        };
        mac.update(signed_payload.as_bytes());
        mac.verify_slice(&expected).is_ok()
    }

    /// Format `recentHistory` from the webhook payload as a chronological conversation log.
    fn format_recent_history(
        payload: &serde_json::Value,
        channel_type: &str,
        from: &str,
    ) -> Option<String> {
        let history = payload.get("recentHistory")?.as_array()?;
        if history.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        lines.push(format!(
            "[Conversation history with {from} via {channel_type}]"
        ));
        for entry in history {
            let direction = entry
                .get("direction")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let body = entry.get("body").and_then(|v| v.as_str()).unwrap_or("");
            if body.is_empty() {
                continue;
            }
            let time = entry
                .get("receivedAt")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let speaker = if direction == "inbound" {
                from
            } else {
                "Agent"
            };
            // Trim to "2025-01-15T12:00" for readability
            let short_time = &time[..16.min(time.len())];
            lines.push(format!("[{short_time}] {speaker}: {body}"));
        }
        lines.push("[Current message]".to_string());
        Some(lines.join("\n"))
    }

    /// Extract `conversationState` metadata as a context prefix.
    fn format_conversation_state(payload: &serde_json::Value) -> Option<String> {
        let state = payload.get("conversationState")?;
        if state.is_null() {
            return None;
        }
        Some(format!("[conversation state: {state}] "))
    }

    /// Parse an incoming webhook payload from AgentPhone.
    ///
    /// Handles both `channel: "sms"` and `channel: "voice"` events.
    /// Returns an empty vec for unknown events or unauthorized callers.
    ///
    /// The message content is enriched with:
    /// - `recentHistory`: chronological conversation log prepended to the message
    /// - `conversationState`: custom metadata prefix
    /// - Channel is tagged as `agentphone_sms` or `agentphone_voice` for session scoping
    pub fn parse_webhook_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        let event = payload.get("event").and_then(|v| v.as_str()).unwrap_or("");
        if event != "agent.message" {
            tracing::debug!("AgentPhone: ignoring non-message event: {event}");
            return messages;
        }

        let channel = payload
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let Some(data) = payload.get("data") else {
            return messages;
        };

        let from = data.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = data.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if from.is_empty() {
            return messages;
        }

        // Normalize phone numbers to E.164
        let normalized_from = if from.starts_with('+') {
            from.to_string()
        } else {
            format!("+{from}")
        };

        // Resolve the counterparty: if `from` is the agent's own number, the
        // counterparty is `to`; otherwise it's `from`.
        let counterparty = if let Some(ref agent_num) = self.agent_phone_number {
            if normalized_from == *agent_num {
                let normalized_to = if to.starts_with('+') {
                    to.to_string()
                } else {
                    format!("+{to}")
                };
                if normalized_to.is_empty() || normalized_to == "+" {
                    normalized_from.clone()
                } else {
                    normalized_to
                }
            } else {
                normalized_from.clone()
            }
        } else {
            normalized_from.clone()
        };

        // Check allowlist or active outbound session against the original sender
        if !self.should_accept_inbound(&normalized_from) {
            tracing::warn!(
                "AgentPhone: ignoring message from unauthorized number: {normalized_from}. \
                Add to channels_config.agentphone.allowed_numbers in config.toml."
            );
            return messages;
        }

        let raw_content = match channel {
            "sms" => data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "voice" => {
                let transcript = data
                    .get("transcript")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if transcript.is_empty() {
                    return messages;
                }
                let confidence = data
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if confidence > 0.0 {
                    format!("[voice call, confidence: {confidence:.2}] {transcript}")
                } else {
                    format!("[voice call] {transcript}")
                }
            }
            other => {
                tracing::debug!("AgentPhone: ignoring unknown channel type: {other}");
                return messages;
            }
        };

        if raw_content.is_empty() {
            return messages;
        }

        // Build enriched content with conversation history and state
        let mut content_parts = Vec::new();

        // 1. Recent conversation history (chronological log from AgentPhone)
        if let Some(history) = Self::format_recent_history(payload, channel, &counterparty) {
            content_parts.push(history);
        }

        // 2. Conversation state metadata
        if let Some(state) = Self::format_conversation_state(payload) {
            content_parts.push(state);
        }

        // 3. The actual message
        content_parts.push(raw_content);

        let content = content_parts.join("\n");

        let timestamp = payload
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|t| {
                chrono::DateTime::parse_from_rfc3339(t)
                    .ok()
                    .map(|dt| dt.timestamp().cast_unsigned())
            })
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            });

        // Use conversationId or callId as thread ID for context tracking
        let thread_ts = data
            .get("conversationId")
            .or_else(|| data.get("callId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Tag channel as agentphone_sms or agentphone_voice for session scoping
        let typed_channel = format!("agentphone_{channel}");

        messages.push(ChannelMessage {
            id: Uuid::new_v4().to_string(),
            reply_target: counterparty.clone(),
            sender: counterparty,
            content,
            channel: typed_channel,
            timestamp,
            thread_ts,
            interruption_scope_id: None,
            attachments: vec![],
        });

        messages
    }
}

#[async_trait]
impl Channel for AgentPhoneChannel {
    fn name(&self) -> &str {
        "agentphone"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Send outbound SMS via AgentPhone API.
        // We need an agent_id and a number to send from.
        let agent_id = self
            .agent_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("AgentPhone: agent_id required to send messages"))?;

        // Check outbound allowlist or active outbound session
        if !self.is_number_allowed(&message.recipient) && !self.has_active_outbound(&message.recipient) {
            anyhow::bail!(
                "AgentPhone: recipient {} is not in allowed_numbers",
                message.recipient
            );
        }

        // Look up conversations for this agent to find an existing conversation
        // with this recipient, or send via the number's messages endpoint.
        let url = format!("{API_BASE}/agents/{agent_id}/conversations");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .query(&[("limit", "100")])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("AgentPhone: failed to list conversations: {status} — {error_body}");
        }

        let conversations: serde_json::Value = resp.json().await?;

        // Find existing conversation with this participant
        let conversation_id = conversations
            .as_array()
            .and_then(|convs| {
                convs.iter().find_map(|c| {
                    let participant = c.get("participant").and_then(|v| v.as_str())?;
                    if participant == message.recipient
                        || participant == message.recipient.strip_prefix('+').unwrap_or("")
                    {
                        c.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
            });

        if let Some(conv_id) = conversation_id {
            // Reply via conversation — the API will send as SMS
            let url = format!("{API_BASE}/conversations/{conv_id}");
            let body = serde_json::json!({
                "metadata": {
                    "_reply": &message.content,
                }
            });
            let resp = self
                .http
                .patch(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let error_body = resp.text().await.unwrap_or_default();
                tracing::error!("AgentPhone send failed: {status} — {error_body}");
                anyhow::bail!("AgentPhone API error: {status}");
            }
        } else {
            tracing::warn!(
                "AgentPhone: no existing conversation found with {}. \
                Cannot initiate SMS without a prior inbound message.",
                message.recipient
            );
        }

        Ok(())
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        // AgentPhone uses webhooks (push-based), not polling.
        // Messages are received via the gateway's /agentphone endpoint.
        tracing::info!(
            "AgentPhone channel active (webhook mode). \
            Configure AgentPhone webhook to POST to your gateway's /agentphone endpoint."
        );

        // Sync agent voice/greeting config on startup
        if let Err(e) = self.sync_agent_config().await {
            tracing::warn!("AgentPhone: failed to sync agent config on startup: {e}");
        }

        // Keep the task alive — it will be cancelled when the channel shuts down
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }

    async fn health_check(&self) -> bool {
        let url = format!("{API_BASE}/agents");
        self.http
            .get(&url)
            .bearer_auth(&self.api_key)
            .query(&[("limit", "1")])
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PhoneNumberConfig;

    fn make_channel() -> AgentPhoneChannel {
        AgentPhoneChannel::new(
            "test-api-key".into(),
            "test-webhook-secret".into(),
            vec![PhoneNumberEntry::Simple("+15551234567".into())],
        )
        .with_agent_id(Some("agt_test123".into()))
    }

    #[test]
    fn channel_name() {
        let ch = make_channel();
        assert_eq!(ch.name(), "agentphone");
    }

    #[test]
    fn is_number_allowed_exact_match() {
        let ch = make_channel();
        assert!(ch.is_number_allowed("+15551234567"));
        assert!(!ch.is_number_allowed("+15559999999"));
    }

    #[test]
    fn is_number_allowed_wildcard() {
        let ch = AgentPhoneChannel::new(
            "key".into(),
            "secret".into(),
            vec![PhoneNumberEntry::Simple("*".into())],
        );
        assert!(ch.is_number_allowed("+15559999999"));
    }

    #[test]
    fn is_number_allowed_empty_denies_all() {
        let ch = AgentPhoneChannel::new("key".into(), "secret".into(), vec![]);
        assert!(!ch.is_number_allowed("+15551234567"));
    }

    #[test]
    fn trusted_number_check() {
        let ch = AgentPhoneChannel::new(
            "key".into(),
            "secret".into(),
            vec![
                PhoneNumberEntry::Simple("+15551234567".into()),
                PhoneNumberEntry::Detailed(PhoneNumberConfig {
                    number: "+15559876543".into(),
                    trust: PhoneTrustLevel::Scoped,
                    purpose: Some("dentist appointment".into()),
                }),
            ],
        );
        assert!(ch.is_trusted_number("+15551234567"));
        assert!(!ch.is_trusted_number("+15559876543"));
        assert!(!ch.is_trusted_number("+15550000000"));
    }

    #[test]
    fn get_trust_level_and_purpose() {
        let ch = AgentPhoneChannel::new(
            "key".into(),
            "secret".into(),
            vec![
                PhoneNumberEntry::Simple("+15551234567".into()),
                PhoneNumberEntry::Detailed(PhoneNumberConfig {
                    number: "+15559876543".into(),
                    trust: PhoneTrustLevel::Scoped,
                    purpose: Some("appointment scheduling".into()),
                }),
            ],
        );
        assert_eq!(ch.get_trust_level("+15551234567"), Some(PhoneTrustLevel::Trusted));
        assert_eq!(ch.get_trust_level("+15559876543"), Some(PhoneTrustLevel::Scoped));
        assert_eq!(ch.get_trust_level("+15550000000"), None);
        assert_eq!(ch.get_number_purpose("+15551234567"), None);
        assert_eq!(ch.get_number_purpose("+15559876543"), Some("appointment scheduling"));
    }

    #[test]
    fn outbound_session_tracking() {
        let ch = make_channel();
        let number = "+15559999999";

        // No session
        assert!(!ch.has_active_outbound(number));

        // Register session
        ch.register_outbound(number, "call_123", Some("dentist"), true);
        assert!(ch.has_active_outbound(number));
        assert_eq!(ch.get_outbound_purpose(number), Some("dentist".to_string()));

        // Clear session
        ch.clear_outbound(number);
        assert!(!ch.has_active_outbound(number));
    }

    #[test]
    fn should_accept_inbound_uses_allowlist_only() {
        let ch = make_channel();

        // Allowed number is accepted
        assert!(ch.should_accept_inbound("+15551234567"));

        // Unknown number is rejected even with active outbound session
        let number = "+15559999999";
        ch.register_outbound(number, "call_123", None, true);
        assert!(!ch.should_accept_inbound(number));
    }

    #[test]
    fn parse_sms_event() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.message",
            "channel": "sms",
            "timestamp": "2025-01-15T12:00:00Z",
            "agentId": "agt_test123",
            "data": {
                "conversationId": "conv_def456",
                "numberId": "num_xyz789",
                "from": "+15551234567",
                "to": "+15550001111",
                "message": "Hello, I need help",
                "direction": "inbound",
                "receivedAt": "2025-01-15T12:00:00Z"
            }
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].sender, "+15551234567");
        assert!(messages[0].content.contains("Hello, I need help"));
        assert_eq!(messages[0].channel, "agentphone_sms");
        assert_eq!(
            messages[0].thread_ts.as_deref(),
            Some("conv_def456")
        );
    }

    #[test]
    fn parse_voice_event() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.message",
            "channel": "voice",
            "timestamp": "2025-01-15T14:00:05Z",
            "agentId": "agt_test123",
            "data": {
                "callId": "call_abc123",
                "numberId": "num_xyz789",
                "from": "+15551234567",
                "to": "+15550001111",
                "status": "in-progress",
                "transcript": "I need help with my order",
                "confidence": 0.95,
                "direction": "inbound"
            }
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("I need help with my order"));
        assert!(messages[0].content.contains("0.95"));
        assert_eq!(messages[0].channel, "agentphone_voice");
        assert_eq!(messages[0].thread_ts.as_deref(), Some("call_abc123"));
    }

    #[test]
    fn parse_unauthorized_number_dropped() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.message",
            "channel": "sms",
            "timestamp": "2025-01-15T12:00:00Z",
            "data": {
                "from": "+15559999999",
                "message": "Should be rejected"
            }
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert!(messages.is_empty());
    }

    #[test]
    fn parse_unknown_event_ignored() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.status",
            "channel": "sms",
            "data": {}
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert!(messages.is_empty());
    }

    #[test]
    fn verify_signature_no_secret_accepts_all() {
        let ch = AgentPhoneChannel::new("key".into(), String::new(), Vec::new());
        assert!(ch.verify_signature(b"body", None, None));
    }

    #[test]
    fn verify_signature_missing_header_rejects() {
        let ch = make_channel();
        assert!(!ch.verify_signature(b"body", None, None));
    }

    #[test]
    fn verify_signature_valid() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret = "test-webhook-secret";
        let ch = AgentPhoneChannel::new("key".into(), secret.into(), Vec::new());

        // Use current timestamp to avoid replay protection rejection
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let timestamp = now.to_string();
        let body = b"test body";
        let signed_payload = format!("{timestamp}.test body");

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(signed_payload.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());

        assert!(ch.verify_signature(
            body,
            Some(&format!("sha256={sig}")),
            Some(&timestamp),
        ));
    }

    #[test]
    fn verify_signature_invalid_rejected() {
        let ch = make_channel();
        assert!(!ch.verify_signature(
            b"body",
            Some("sha256=deadbeef"),
            Some("1705312800"),
        ));
    }

    #[test]
    fn parse_sms_with_recent_history() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.message",
            "channel": "sms",
            "timestamp": "2025-01-15T12:05:00Z",
            "data": {
                "conversationId": "conv_1",
                "from": "+15551234567",
                "message": "Can I move it to Thursday?",
                "direction": "inbound"
            },
            "recentHistory": [
                {
                    "body": "Hi, this is a reminder about your appointment tomorrow at 2pm.",
                    "direction": "outbound",
                    "receivedAt": "2025-01-15T11:58:00Z"
                },
                {
                    "body": "Thanks, can I reschedule?",
                    "direction": "inbound",
                    "receivedAt": "2025-01-15T12:00:00Z"
                }
            ]
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        let content = &messages[0].content;
        assert!(content.contains("[Conversation history with +15551234567 via sms]"));
        assert!(content.contains("Agent: Hi, this is a reminder"));
        assert!(content.contains("+15551234567: Thanks, can I reschedule?"));
        assert!(content.contains("[Current message]"));
        assert!(content.contains("Can I move it to Thursday?"));
    }

    #[test]
    fn parse_sms_with_conversation_state() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.message",
            "channel": "sms",
            "timestamp": "2025-01-15T12:00:00Z",
            "data": {
                "from": "+15551234567",
                "message": "Yes please"
            },
            "conversationState": {
                "customerName": "Jane Doe",
                "orderId": "ORD-12345"
            }
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("[conversation state:"));
        assert!(messages[0].content.contains("Jane Doe"));
        assert!(messages[0].content.contains("Yes please"));
    }

    #[test]
    fn parse_no_recent_history_still_works() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event": "agent.message",
            "channel": "sms",
            "timestamp": "2025-01-15T12:00:00Z",
            "data": {
                "from": "+15551234567",
                "message": "Hello"
            }
        });

        let messages = ch.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[0].channel, "agentphone_sms");
    }
}
