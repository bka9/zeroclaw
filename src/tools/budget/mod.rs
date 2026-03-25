pub mod traits;
pub mod types;
pub mod ynab;

use crate::security::{policy::ToolOperation, SecurityPolicy};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use traits::BudgetProvider;
use types::{CategoryInput, TransactionInput, TransactionListParams, TransactionUpdateInput};

const MAX_ERROR_BODY_CHARS: usize = 500;

/// Tool for budget and financial-planning operations.
///
/// Supports 15 actions gated by `[budget].allowed_actions` in config:
/// - `budgets.list`        — list all budgets
/// - `budgets.get`         — get a specific budget
/// - `budgets.summary`     — get budget month summary
/// - `transactions.list`   — list transactions (filterable by account, category, payee, date)
/// - `transactions.create` — create a transaction
/// - `transactions.update` — update a transaction
/// - `transactions.import` — import transactions from linked accounts
/// - `user.get`            — get authenticated user info
/// - `accounts.list`       — list all accounts
/// - `accounts.get`        — get a specific account
/// - `categories.list`     — list all categories
/// - `categories.get`      — get a specific category
/// - `categories.create`   — create a category
/// - `payees.list`         — list all payees
/// - `payees.get`          — get a specific payee
pub struct BudgetTool {
    provider: Box<dyn BudgetProvider>,
    allowed_actions: Vec<String>,
    security: Arc<SecurityPolicy>,
    default_budget_id: Option<String>,
}

impl BudgetTool {
    pub fn new(
        provider: Box<dyn BudgetProvider>,
        allowed_actions: Vec<String>,
        security: Arc<SecurityPolicy>,
        default_budget_id: Option<String>,
    ) -> Self {
        Self {
            provider,
            allowed_actions,
            security,
            default_budget_id,
        }
    }

    fn is_action_allowed(&self, action: &str) -> bool {
        self.allowed_actions.iter().any(|a| a == action)
    }

    /// Resolve `budget_id` from args, falling back to the configured default.
    fn resolve_budget_id(&self, args: &Value) -> anyhow::Result<String> {
        if let Some(id) = args["budget_id"].as_str().filter(|s| !s.is_empty()) {
            return Ok(id.to_string());
        }
        self.default_budget_id.clone().ok_or_else(|| {
            anyhow::anyhow!("missing required parameter: budget_id (no default configured)")
        })
    }

    // ── Action handlers ─────────────────────────────────────────

    async fn handle_budgets_list(&self) -> anyhow::Result<ToolResult> {
        let data = self.provider.budgets_list().await?;
        Ok(success_result(data))
    }

    async fn handle_budgets_get(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let data = self.provider.budgets_get(&id).await?;
        Ok(success_result(data))
    }

    async fn handle_budgets_summary(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let month = args["month"].as_str();
        let data = self.provider.budgets_summary(&id, month).await?;
        Ok(success_result(data))
    }

    async fn handle_transactions_list(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let params = TransactionListParams {
            budget_id: self.resolve_budget_id(args)?,
            since_date: args["since_date"].as_str().map(String::from),
            transaction_type: args["transaction_type"].as_str().map(String::from),
            account_id: args["account_id"].as_str().map(String::from),
            category_id: args["category_id"].as_str().map(String::from),
            payee_id: args["payee_id"].as_str().map(String::from),
            last_knowledge_of_server: args["last_knowledge_of_server"].as_i64(),
        };
        let data = self.provider.transactions_list(&params).await?;
        Ok(success_result(data))
    }

    async fn handle_transactions_create(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let txn = parse_transaction_input(args)?;
        let data = self.provider.transactions_create(&id, &txn).await?;
        Ok(success_result(data))
    }

    async fn handle_transactions_update(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let transaction_id = args["transaction_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: transaction_id"))?;
        let txn = parse_transaction_update_input(args);
        let data = self
            .provider
            .transactions_update(&id, transaction_id, &txn)
            .await?;
        Ok(success_result(data))
    }

    async fn handle_transactions_import(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let data = self.provider.transactions_import(&id).await?;
        Ok(success_result(data))
    }

    async fn handle_user_get(&self) -> anyhow::Result<ToolResult> {
        let data = self.provider.user_get().await?;
        Ok(success_result(data))
    }

    async fn handle_accounts_list(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let data = self.provider.accounts_list(&id).await?;
        Ok(success_result(data))
    }

    async fn handle_accounts_get(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let account_id = args["account_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: account_id"))?;
        let data = self.provider.accounts_get(&id, account_id).await?;
        Ok(success_result(data))
    }

    async fn handle_categories_list(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let data = self.provider.categories_list(&id).await?;
        Ok(success_result(data))
    }

    async fn handle_categories_get(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let category_id = args["category_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: category_id"))?;
        let data = self.provider.categories_get(&id, category_id).await?;
        Ok(success_result(data))
    }

    async fn handle_categories_create(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let category_group_id = args["category_group_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: category_group_id"))?;
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: name"))?;
        let input = CategoryInput {
            category_group_id: category_group_id.to_string(),
            name: name.to_string(),
        };
        let data = self.provider.categories_create(&id, &input).await?;
        Ok(success_result(data))
    }

    async fn handle_payees_list(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let data = self.provider.payees_list(&id).await?;
        Ok(success_result(data))
    }

    async fn handle_payees_get(&self, args: &Value) -> anyhow::Result<ToolResult> {
        let id = self.resolve_budget_id(args)?;
        let payee_id = args["payee_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: payee_id"))?;
        let data = self.provider.payees_get(&id, payee_id).await?;
        Ok(success_result(data))
    }
}

// ── Tool trait impl ─────────────────────────────────────────────

#[async_trait]
impl Tool for BudgetTool {
    fn name(&self) -> &str {
        "budget"
    }

    fn description(&self) -> &str {
        "Manage budgets, transactions, accounts, categories, and payees via a \
         financial planning service. All monetary amounts are in milliunits \
         (1/1000 of the currency unit, e.g. $1.50 = 1500)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "budgets.list", "budgets.get", "budgets.summary",
                        "transactions.list", "transactions.create",
                        "transactions.update", "transactions.import",
                        "user.get",
                        "accounts.list", "accounts.get",
                        "categories.list", "categories.get", "categories.create",
                        "payees.list", "payees.get"
                    ],
                    "description": "The budget action to perform."
                },
                "budget_id": {
                    "type": "string",
                    "description": "Budget ID (UUID, 'last-used', or 'default'). Falls back to configured default."
                },
                "transaction_id": {
                    "type": "string",
                    "description": "Transaction ID for transactions.update."
                },
                "account_id": {
                    "type": "string",
                    "description": "Account ID for accounts.get or transaction filtering."
                },
                "category_id": {
                    "type": "string",
                    "description": "Category ID for categories.get or transaction filtering."
                },
                "category_group_id": {
                    "type": "string",
                    "description": "Category group ID for categories.create."
                },
                "payee_id": {
                    "type": "string",
                    "description": "Payee ID for payees.get or transaction filtering."
                },
                "payee_name": {
                    "type": "string",
                    "description": "Payee name for transactions.create/update."
                },
                "since_date": {
                    "type": "string",
                    "description": "Filter transactions since this date (YYYY-MM-DD)."
                },
                "transaction_type": {
                    "type": "string",
                    "enum": ["uncategorized", "unapproved"],
                    "description": "Filter transaction type."
                },
                "date": {
                    "type": "string",
                    "description": "Transaction date (YYYY-MM-DD) for create/update."
                },
                "amount": {
                    "type": "integer",
                    "description": "Amount in milliunits (1/1000 currency unit). Negative = outflow."
                },
                "memo": {
                    "type": "string",
                    "description": "Transaction memo."
                },
                "cleared": {
                    "type": "string",
                    "enum": ["cleared", "uncleared", "reconciled"],
                    "description": "Transaction cleared status."
                },
                "approved": {
                    "type": "boolean",
                    "description": "Whether the transaction is approved."
                },
                "flag_color": {
                    "type": "string",
                    "enum": ["red", "orange", "yellow", "green", "blue", "purple"],
                    "description": "Transaction flag color."
                },
                "month": {
                    "type": "string",
                    "description": "Month (YYYY-MM-DD first-of-month or 'current') for budgets.summary."
                },
                "name": {
                    "type": "string",
                    "description": "Name for categories.create."
                },
                "last_knowledge_of_server": {
                    "type": "integer",
                    "description": "Delta sync token from a previous list response."
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
                     Update [budget].allowed_actions in config to enable it."
                )),
            });
        }

        // Rate limit check.
        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded for budget tool.".into()),
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
            "transactions.create"
            | "transactions.update"
            | "transactions.import"
            | "categories.create" => ToolOperation::Act,
            _ => ToolOperation::Read,
        };
        if let Err(error) = self.security.enforce_tool_operation(op, "budget") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let result = match action {
            "budgets.list" => self.handle_budgets_list().await,
            "budgets.get" => self.handle_budgets_get(&args).await,
            "budgets.summary" => self.handle_budgets_summary(&args).await,
            "transactions.list" => self.handle_transactions_list(&args).await,
            "transactions.create" => self.handle_transactions_create(&args).await,
            "transactions.update" => self.handle_transactions_update(&args).await,
            "transactions.import" => self.handle_transactions_import(&args).await,
            "user.get" => self.handle_user_get().await,
            "accounts.list" => self.handle_accounts_list(&args).await,
            "accounts.get" => self.handle_accounts_get(&args).await,
            "categories.list" => self.handle_categories_list(&args).await,
            "categories.get" => self.handle_categories_get(&args).await,
            "categories.create" => self.handle_categories_create(&args).await,
            "payees.list" => self.handle_payees_list(&args).await,
            "payees.get" => self.handle_payees_get(&args).await,
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

fn parse_transaction_input(args: &Value) -> anyhow::Result<TransactionInput> {
    let account_id = args["account_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: account_id"))?
        .to_string();
    let date = args["date"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: date"))?
        .to_string();
    let amount = args["amount"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: amount"))?;

    Ok(TransactionInput {
        account_id,
        date,
        amount,
        payee_name: args["payee_name"].as_str().map(String::from),
        payee_id: args["payee_id"].as_str().map(String::from),
        category_id: args["category_id"].as_str().map(String::from),
        memo: args["memo"].as_str().map(String::from),
        cleared: args["cleared"].as_str().map(String::from),
        approved: args["approved"].as_bool(),
        flag_color: args["flag_color"].as_str().map(String::from),
    })
}

fn parse_transaction_update_input(args: &Value) -> TransactionUpdateInput {
    TransactionUpdateInput {
        account_id: args["account_id"].as_str().map(String::from),
        date: args["date"].as_str().map(String::from),
        amount: args["amount"].as_i64(),
        payee_name: args["payee_name"].as_str().map(String::from),
        payee_id: args["payee_id"].as_str().map(String::from),
        category_id: args["category_id"].as_str().map(String::from),
        memo: args["memo"].as_str().map(String::from),
        cleared: args["cleared"].as_str().map(String::from),
        approved: args["approved"].as_bool(),
        flag_color: args["flag_color"].as_str().map(String::from),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
