use super::types::{CategoryInput, TransactionInput, TransactionListParams};
use async_trait::async_trait;
use serde_json::Value;

/// Abstract interface for budget/financial-planning providers.
///
/// Each method returns provider-native JSON wrapped in `anyhow::Result`.
/// The [`super::BudgetTool`] wrapper converts results into [`ToolResult`].
#[async_trait]
pub trait BudgetProvider: Send + Sync {
    // ── Budgets ─────────────────────────────────────────────────
    async fn budgets_list(&self) -> anyhow::Result<Value>;
    async fn budgets_get(&self, budget_id: &str) -> anyhow::Result<Value>;
    async fn budgets_summary(&self, budget_id: &str, month: Option<&str>) -> anyhow::Result<Value>;

    // ── Transactions ────────────────────────────────────────────
    async fn transactions_list(&self, params: &TransactionListParams) -> anyhow::Result<Value>;
    async fn transactions_create(
        &self,
        budget_id: &str,
        txn: &TransactionInput,
    ) -> anyhow::Result<Value>;
    async fn transactions_update(
        &self,
        budget_id: &str,
        transaction_id: &str,
        txn: &TransactionInput,
    ) -> anyhow::Result<Value>;
    async fn transactions_import(&self, budget_id: &str) -> anyhow::Result<Value>;

    // ── User ────────────────────────────────────────────────────
    async fn user_get(&self) -> anyhow::Result<Value>;

    // ── Accounts ────────────────────────────────────────────────
    async fn accounts_list(&self, budget_id: &str) -> anyhow::Result<Value>;
    async fn accounts_get(&self, budget_id: &str, account_id: &str) -> anyhow::Result<Value>;

    // ── Categories ──────────────────────────────────────────────
    async fn categories_list(&self, budget_id: &str) -> anyhow::Result<Value>;
    async fn categories_get(&self, budget_id: &str, category_id: &str) -> anyhow::Result<Value>;
    async fn categories_create(
        &self,
        budget_id: &str,
        category: &CategoryInput,
    ) -> anyhow::Result<Value>;

    // ── Payees ──────────────────────────────────────────────────
    async fn payees_list(&self, budget_id: &str) -> anyhow::Result<Value>;
    async fn payees_get(&self, budget_id: &str, payee_id: &str) -> anyhow::Result<Value>;
}
