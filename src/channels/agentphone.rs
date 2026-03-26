use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use uuid::Uuid;

const API_BASE: &str = "https://api.agentphone.to/v1";

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
    allowed_numbers: Vec<String>,
    voice: Option<String>,
    begin_message: Option<String>,
    proxy_url: Option<String>,
    http: reqwest::Client,
}

impl AgentPhoneChannel {
    pub fn new(
        api_key: String,
        webhook_secret: String,
        allowed_numbers: Vec<String>,
    ) -> Self {
        Self {
            api_key,
            webhook_secret,
            allowed_numbers,
            agent_id: None,
            voice: None,
            begin_message: None,
            proxy_url: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn with_agent_id(mut self, agent_id: Option<String>) -> Self {
        self.agent_id = agent_id;
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

    /// Check if a phone number is allowed (E.164 format: +1234567890).
    pub fn is_number_allowed(&self, phone: &str) -> bool {
        self.allowed_numbers.iter().any(|n| n == "*" || n == phone)
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

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "AgentPhone: failed to sync agent config: {status} — {error_body}"
            );
        } else {
            tracing::info!("AgentPhone: synced agent {agent_id} voice/greeting config");
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

    /// Parse an incoming webhook payload from AgentPhone.
    ///
    /// Handles both `channel: "sms"` and `channel: "voice"` events.
    /// Returns an empty vec for unknown events or unauthorized callers.
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
        if from.is_empty() {
            return messages;
        }

        // Normalize phone number to E.164
        let normalized_from = if from.starts_with('+') {
            from.to_string()
        } else {
            format!("+{from}")
        };

        // Check allowlist
        if !self.is_number_allowed(&normalized_from) {
            tracing::warn!(
                "AgentPhone: ignoring message from unauthorized number: {normalized_from}. \
                Add to channels_config.agentphone.allowed_numbers in config.toml."
            );
            return messages;
        }

        let content = match channel {
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

        if content.is_empty() {
            return messages;
        }

        let timestamp = payload
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|t| {
                chrono::DateTime::parse_from_rfc3339(t)
                    .ok()
                    .map(|dt| dt.timestamp() as u64)
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

        messages.push(ChannelMessage {
            id: Uuid::new_v4().to_string(),
            reply_target: normalized_from.clone(),
            sender: normalized_from,
            content,
            channel: "agentphone".to_string(),
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

        // Check outbound allowlist
        if !self.is_number_allowed(&message.recipient) {
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

    fn make_channel() -> AgentPhoneChannel {
        AgentPhoneChannel::new(
            "test-api-key".into(),
            "test-webhook-secret".into(),
            vec!["+15551234567".into()],
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
            vec!["*".into()],
        );
        assert!(ch.is_number_allowed("+15559999999"));
    }

    #[test]
    fn is_number_allowed_empty_denies_all() {
        let ch = AgentPhoneChannel::new("key".into(), "secret".into(), vec![]);
        assert!(!ch.is_number_allowed("+15551234567"));
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
        assert_eq!(messages[0].content, "Hello, I need help");
        assert_eq!(messages[0].channel, "agentphone");
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
        let ch = AgentPhoneChannel::new("key".into(), String::new(), vec![]);
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
        let ch = AgentPhoneChannel::new("key".into(), secret.into(), vec![]);

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
}
