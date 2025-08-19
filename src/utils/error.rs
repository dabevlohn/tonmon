use thiserror::Error;

#[derive(Error, Debug)]
pub enum ServiceError {
    #[error("TON client error: {0}")]
    TonClient(#[from] anyhow::Error),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Subscription not found: {0}")]
    SubscriptionNotFound(String),

    #[error("Transaction trace not found: {0}")]
    TransactionTraceNotFound(String),
}

pub type Result<T> = std::result::Result<T, ServiceError>;
