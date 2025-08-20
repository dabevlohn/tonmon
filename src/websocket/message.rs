use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Сообщения от клиента к серверу
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Подписка на обновления адреса
    Subscribe {
        address: String,
        subscription_id: Option<String>,
    },
    /// Отписка от обновлений адреса
    Unsubscribe { subscription_id: String },
    /// Пинг для проверки соединения
    Ping,
}

/// Сообщения от сервера к клиенту
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Подтверждение подписки
    SubscriptionConfirm {
        subscription_id: String,
        address: String,
    },
    /// Подтверждение отписки
    UnsubscriptionConfirm { subscription_id: String },
    /// Новая транзакция и её трейс
    TransactionTrace {
        subscription_id: String,
        address: String,
        trace: Box<TransactionTrace>,
    },
    /// Ошибка
    Error {
        message: String,
        subscription_id: Option<String>,
    },
    /// Понг в ответ на пинг
    Pong,
}

/// Данные о трейсе транзакции
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionTrace {
    pub trace_id: String,
    pub root_hash: String,
    pub account: String,
    pub lt: String,
    pub hash: String,
    pub now: u32,
    pub in_msg: Option<MessageInfo>,
    pub out_msgs: Vec<MessageInfo>,
    pub children: Vec<TransactionTrace>, // Дочерние транзакции в трейсе
    pub total_fees: String,
    pub compute_phase: Option<ComputePhaseInfo>,
    pub action_phase: Option<ActionPhaseInfo>,
}

/// Информация о сообщении
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageInfo {
    pub hash: String,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub value: String,
    pub body: Option<String>,
    pub message_type: String, // "internal", "external-in", "external-out"
}

/// Информация о фазе вычислений
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputePhaseInfo {
    pub success: bool,
    pub gas_used: String,
    pub gas_limit: String,
    pub exit_code: Option<i32>,
}

/// Информация о фазе действий
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPhaseInfo {
    pub success: bool,
    pub total_actions: u32,
    pub valid_actions: u32,
    pub no_funds: bool,
}

impl ServerMessage {
    pub fn error(message: String, subscription_id: Option<String>) -> Self {
        Self::Error {
            message,
            subscription_id,
        }
    }

    pub fn subscription_confirm(address: String) -> Self {
        let subscription_id = Uuid::new_v4().to_string();
        Self::SubscriptionConfirm {
            subscription_id,
            address,
        }
    }
}
