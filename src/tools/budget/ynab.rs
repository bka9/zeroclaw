use super::traits::BudgetProvider;
use super::types::{CategoryInput, TransactionInput, TransactionListParams, TransactionUpdateInput};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const YNAB_BASE: &str = "https://api.ynab.com/v1";
const MAX_ERROR_BODY_CHARS: usize = 500;

/// YNAB (You Need A Budget) implementation of [`BudgetProvider`].
///
/// Translates the generic "budget" vocabulary to YNAB's "plan" endpoints.
/// All monetary amounts are in **milliunits** (1/1000 of the currency unit).
pub struct YnabProvider {
    api_token: String,
    http: Client,
}

impl YnabProvider {
    pub fn new(api_token: String, timeout_secs: u64) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client for YNAB provider");

        Self { api_token, http }
    }

    // ── HTTP helpers ────────────────────────────────────────────

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Value> {
        let url = format!("{YNAB_BASE}{path}");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_token)
            .query(query)
            .send()
            .await?;
        self.parse_response(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let url = format!("{YNAB_BASE}{path}");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_token)
            .json(body)
            .send()
            .await?;
        self.parse_response(resp).await
    }

    async fn put(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let url = format!("{YNAB_BASE}{path}");
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.api_token)
            .json(body)
            .send()
            .await?;
        self.parse_response(resp).await
    }

    /// Parse the YNAB response: unwrap the `data` envelope on success,
    /// extract the error detail on failure.
    async fn parse_response(&self, resp: reqwest::Response) -> anyhow::Result<Value> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["detail"].as_str().map(String::from))
                .unwrap_or_else(|| truncate(&body, MAX_ERROR_BODY_CHARS));
            anyhow::bail!("YNAB API error ({status}): {detail}");
        }

        let json: Value = serde_json::from_str(&body)?;
        // YNAB wraps all successful responses in {"data": {...}}
        Ok(json.get("data").cloned().unwrap_or(json))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// Percent-encode a user-supplied path segment to prevent path traversal.
fn encode_segment(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

#[async_trait]
impl BudgetProvider for YnabProvider {
    // ── Budgets ─────────────────────────────────────────────────

    async fn budgets_list(&self) -> anyhow::Result<Value> {
        self.get("/plans", &[("include_accounts", "true")]).await
    }

    async fn budgets_get(&self, budget_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        self.get(&format!("/plans/{id}"), &[]).await
    }

    async fn budgets_summary(&self, budget_id: &str, month: Option<&str>) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        match month {
            Some(m) => {
                let m = encode_segment(m);
                self.get(&format!("/plans/{id}/months/{m}"), &[]).await
            }
            None => self.get(&format!("/plans/{id}/months"), &[]).await,
        }
    }

    // ── Transactions ────────────────────────────────────────────

    async fn transactions_list(&self, params: &TransactionListParams) -> anyhow::Result<Value> {
        let id = encode_segment(&params.budget_id);

        // Use the most specific endpoint based on which filter is set.
        let path = if let Some(ref acct) = params.account_id {
            format!("/plans/{id}/accounts/{}/transactions", encode_segment(acct))
        } else if let Some(ref cat) = params.category_id {
            format!(
                "/plans/{id}/categories/{}/transactions",
                encode_segment(cat)
            )
        } else if let Some(ref payee) = params.payee_id {
            format!("/plans/{id}/payees/{}/transactions", encode_segment(payee))
        } else {
            format!("/plans/{id}/transactions")
        };

        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(ref since) = params.since_date {
            query.push(("since_date", since.as_str()));
        }
        if let Some(ref tt) = params.transaction_type {
            query.push(("type", tt.as_str()));
        }
        let lkos_str;
        if let Some(lkos) = params.last_knowledge_of_server {
            lkos_str = lkos.to_string();
            query.push(("last_knowledge_of_server", &lkos_str));
        }

        self.get(&path, &query).await
    }

    async fn transactions_create(
        &self,
        budget_id: &str,
        txn: &TransactionInput,
    ) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        let mut body = json!({
            "transaction": {
                "account_id": txn.account_id,
                "date": txn.date,
                "amount": txn.amount,
            }
        });
        let txn_obj = body["transaction"].as_object_mut().unwrap();
        if let Some(ref v) = txn.payee_name {
            txn_obj.insert("payee_name".into(), json!(v));
        }
        if let Some(ref v) = txn.payee_id {
            txn_obj.insert("payee_id".into(), json!(v));
        }
        if let Some(ref v) = txn.category_id {
            txn_obj.insert("category_id".into(), json!(v));
        }
        if let Some(ref v) = txn.memo {
            txn_obj.insert("memo".into(), json!(v));
        }
        if let Some(ref v) = txn.cleared {
            txn_obj.insert("cleared".into(), json!(v));
        }
        if let Some(v) = txn.approved {
            txn_obj.insert("approved".into(), json!(v));
        }
        if let Some(ref v) = txn.flag_color {
            txn_obj.insert("flag_color".into(), json!(v));
        }
        self.post(&format!("/plans/{id}/transactions"), &body).await
    }

    async fn transactions_update(
        &self,
        budget_id: &str,
        transaction_id: &str,
        txn: &TransactionUpdateInput,
    ) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        let tid = encode_segment(transaction_id);
        let mut body = json!({ "transaction": {} });
        let txn_obj = body["transaction"].as_object_mut().unwrap();
        if let Some(ref v) = txn.account_id {
            txn_obj.insert("account_id".into(), json!(v));
        }
        if let Some(ref v) = txn.date {
            txn_obj.insert("date".into(), json!(v));
        }
        if let Some(v) = txn.amount {
            txn_obj.insert("amount".into(), json!(v));
        }
        if let Some(ref v) = txn.payee_name {
            txn_obj.insert("payee_name".into(), json!(v));
        }
        if let Some(ref v) = txn.payee_id {
            txn_obj.insert("payee_id".into(), json!(v));
        }
        if let Some(ref v) = txn.category_id {
            txn_obj.insert("category_id".into(), json!(v));
        }
        if let Some(ref v) = txn.memo {
            txn_obj.insert("memo".into(), json!(v));
        }
        if let Some(ref v) = txn.cleared {
            txn_obj.insert("cleared".into(), json!(v));
        }
        if let Some(v) = txn.approved {
            txn_obj.insert("approved".into(), json!(v));
        }
        if let Some(ref v) = txn.flag_color {
            txn_obj.insert("flag_color".into(), json!(v));
        }
        self.put(&format!("/plans/{id}/transactions/{tid}"), &body)
            .await
    }

    async fn transactions_import(&self, budget_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        self.post(&format!("/plans/{id}/transactions/import"), &json!({}))
            .await
    }

    // ── User ────────────────────────────────────────────────────

    async fn user_get(&self) -> anyhow::Result<Value> {
        self.get("/user", &[]).await
    }

    // ── Accounts ────────────────────────────────────────────────

    async fn accounts_list(&self, budget_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        self.get(&format!("/plans/{id}/accounts"), &[]).await
    }

    async fn accounts_get(&self, budget_id: &str, account_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        let aid = encode_segment(account_id);
        self.get(&format!("/plans/{id}/accounts/{aid}"), &[]).await
    }

    // ── Categories ──────────────────────────────────────────────

    async fn categories_list(&self, budget_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        self.get(&format!("/plans/{id}/categories"), &[]).await
    }

    async fn categories_get(&self, budget_id: &str, category_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        let cid = encode_segment(category_id);
        self.get(&format!("/plans/{id}/categories/{cid}"), &[])
            .await
    }

    async fn categories_create(
        &self,
        budget_id: &str,
        category: &CategoryInput,
    ) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        let body = json!({
            "category": {
                "category_group_id": category.category_group_id,
                "name": category.name,
            }
        });
        self.post(&format!("/plans/{id}/categories"), &body).await
    }

    // ── Payees ──────────────────────────────────────────────────

    async fn payees_list(&self, budget_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        self.get(&format!("/plans/{id}/payees"), &[]).await
    }

    async fn payees_get(&self, budget_id: &str, payee_id: &str) -> anyhow::Result<Value> {
        let id = encode_segment(budget_id);
        let pid = encode_segment(payee_id);
        self.get(&format!("/plans/{id}/payees/{pid}"), &[]).await
    }
}
