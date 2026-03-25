use serde::{Deserialize, Serialize};

/// Parameters for filtering transaction list queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransactionListParams {
    pub budget_id: String,
    pub since_date: Option<String>,
    pub transaction_type: Option<String>,
    pub account_id: Option<String>,
    pub category_id: Option<String>,
    pub payee_id: Option<String>,
    pub last_knowledge_of_server: Option<i64>,
}

/// Input fields for creating a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionInput {
    pub account_id: String,
    pub date: String,
    pub amount: i64,
    pub payee_name: Option<String>,
    pub payee_id: Option<String>,
    pub category_id: Option<String>,
    pub memo: Option<String>,
    pub cleared: Option<String>,
    pub approved: Option<bool>,
    pub flag_color: Option<String>,
}

/// Input fields for updating a transaction. All fields are optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionUpdateInput {
    pub account_id: Option<String>,
    pub date: Option<String>,
    pub amount: Option<i64>,
    pub payee_name: Option<String>,
    pub payee_id: Option<String>,
    pub category_id: Option<String>,
    pub memo: Option<String>,
    pub cleared: Option<String>,
    pub approved: Option<bool>,
    pub flag_color: Option<String>,
}

/// Input fields for creating a category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryInput {
    pub category_group_id: String,
    pub name: String,
}
