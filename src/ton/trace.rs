use anyhow::Result;
use reqwest::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::websocket::message::{ActionPhaseInfo, ComputePhaseInfo, MessageInfo, TransactionTrace};

/// Сервис для получения трейсов транзакций
pub struct TraceService {
    http_client: Client,
    api_endpoint: String,
    api_key: Option<String>,
    trace_cache: Arc<RwLock<HashMap<String, TransactionTrace>>>,
}

impl TraceService {
    pub fn new(api_endpoint: String, api_key: Option<String>) -> Self {
        Self {
            http_client: Client::new(),
            api_endpoint,
            api_key,
            trace_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Получает полный трейс транзакции по хешу
    pub async fn get_transaction_trace(&self, tx_hash: &str) -> Result<TransactionTrace> {
        // Сначала проверяем кеш
        {
            let cache = self.trace_cache.read().await;
            if let Some(trace) = cache.get(tx_hash) {
                debug!("Found trace in cache for tx: {}", tx_hash);
                return Ok(trace.clone());
            }
        }

        info!("Fetching trace for transaction: {}", tx_hash);

        // Получаем трейс через TonAPI
        let trace = self.fetch_trace_from_api(tx_hash).await?;

        // Удаляем дубликаты в трейсе по одному и тому же адресу
        let deduplicated_trace = self.deduplicate_trace_by_address(&trace);

        // Кешируем результат
        {
            let mut cache = self.trace_cache.write().await;
            cache.insert(tx_hash.to_string(), deduplicated_trace.clone());
        }

        Ok(deduplicated_trace)
    }

    /// Получает трейс через TonAPI
    async fn fetch_trace_from_api(&self, tx_hash: &str) -> Result<TransactionTrace> {
        let url = format!("{}/v2/traces/{}", self.api_endpoint, tx_hash);

        let mut request = self.http_client.get(&url);

        // Добавляем API ключ если есть
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "API request failed with status: {}",
                response.status()
            ));
        }

        let trace_data: Value = response.json().await?;
        self.parse_trace_from_api_response(trace_data).await
    }

    /// Парсит ответ API в структуру TransactionTrace
    async fn parse_trace_from_api_response(&self, data: Value) -> Result<TransactionTrace> {
        // Это упрощенная версия парсинга. В реальном коде нужно более детальное извлечение
        let transaction = &data["transaction"];

        let hash = transaction["hash"].as_str().unwrap_or_default().to_string();

        let account = transaction["account"]["address"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let lt = transaction["lt"].as_str().unwrap_or("0").to_string();

        let now = transaction["now"].as_u64().unwrap_or(0) as u32;

        let total_fees = transaction["total_fees"]
            .as_str()
            .unwrap_or("0")
            .to_string();

        // Парсим входящие сообщения
        let in_msg = if let Some(in_msg_data) = transaction["in_msg"].as_object() {
            Some(self.parse_message_info(in_msg_data)?)
        } else {
            None
        };

        // Парсим исходящие сообщения
        let out_msgs = if let Some(out_msgs_array) = transaction["out_msgs"].as_array() {
            let mut messages = Vec::new();
            for msg_data in out_msgs_array {
                if let Some(msg_obj) = msg_data.as_object() {
                    messages.push(self.parse_message_info(msg_obj)?);
                }
            }
            messages
        } else {
            Vec::new()
        };

        // Парсим фазы выполнения
        let compute_phase = self.parse_compute_phase(&transaction["compute"])?;
        let action_phase = self.parse_action_phase(&transaction["action"])?;

        // Рекурсивно получаем дочерние транзакции
        let children = self.get_children_transactions(&data).await?;

        let trace = TransactionTrace {
            trace_id: format!("trace_{}", hash),
            root_hash: hash.clone(),
            account,
            lt,
            hash,
            now,
            in_msg,
            out_msgs,
            children,
            total_fees,
            compute_phase,
            action_phase,
        };

        Ok(trace)
    }

    /// Парсит информацию о сообщении
    fn parse_message_info(&self, msg_data: &serde_json::Map<String, Value>) -> Result<MessageInfo> {
        let hash = msg_data
            .get("hash")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let source = msg_data
            .get("source")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let destination = msg_data
            .get("destination")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let value = msg_data
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string();

        let body = msg_data
            .get("body")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let message_type = msg_data
            .get("msg_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(MessageInfo {
            hash,
            source,
            destination,
            value,
            body,
            message_type,
        })
    }

    /// Парсит фазу вычислений
    fn parse_compute_phase(&self, compute_data: &Value) -> Result<Option<ComputePhaseInfo>> {
        if compute_data.is_null() {
            return Ok(None);
        }

        let success = compute_data["success"].as_bool().unwrap_or(false);
        let gas_used = compute_data["gas_used"].as_str().unwrap_or("0").to_string();
        let gas_limit = compute_data["gas_limit"]
            .as_str()
            .unwrap_or("0")
            .to_string();
        let exit_code = compute_data["exit_code"].as_i64().map(|x| x as i32);

        Ok(Some(ComputePhaseInfo {
            success,
            gas_used,
            gas_limit,
            exit_code,
        }))
    }

    /// Парсит фазу действий
    fn parse_action_phase(&self, action_data: &Value) -> Result<Option<ActionPhaseInfo>> {
        if action_data.is_null() {
            return Ok(None);
        }

        let success = action_data["success"].as_bool().unwrap_or(false);
        let total_actions = action_data["total_actions"].as_u64().unwrap_or(0) as u32;
        let valid_actions = action_data["valid_actions"].as_u64().unwrap_or(0) as u32;
        let no_funds = action_data["no_funds"].as_bool().unwrap_or(false);

        Ok(Some(ActionPhaseInfo {
            success,
            total_actions,
            valid_actions,
            no_funds,
        }))
    }

    /// Получает дочерние транзакции (упрощенная версия)
    async fn get_children_transactions(&self, _data: &Value) -> Result<Vec<TransactionTrace>> {
        // В реальной реализации здесь должна быть рекурсивная загрузка
        // всех связанных транзакций в трейсе
        Ok(Vec::new())
    }

    /// Убирает дубликаты транзакций в трейсе по одному и тому же адресу
    fn deduplicate_trace_by_address(&self, trace: &TransactionTrace) -> TransactionTrace {
        let mut seen_addresses = HashSet::new();
        let mut deduplicated_trace = trace.clone();

        // Добавляем корневую транзакцию
        seen_addresses.insert(trace.account.clone());

        // Фильтруем дочерние транзакции
        deduplicated_trace.children = trace
            .children
            .iter()
            .filter(|child| {
                if seen_addresses.contains(&child.account) {
                    debug!(
                        "Removing duplicate transaction for address: {}",
                        child.account
                    );
                    false
                } else {
                    seen_addresses.insert(child.account.clone());
                    true
                }
            })
            .cloned()
            .collect();

        // Рекурсивно дедуплицируем дочерние трейсы
        for child in &mut deduplicated_trace.children {
            *child = self.deduplicate_trace_by_address(child);
        }

        deduplicated_trace
    }

    /// Очищает кеш трейсов
    pub async fn clear_cache(&self) {
        let mut cache = self.trace_cache.write().await;
        cache.clear();
        info!("Trace cache cleared");
    }

    /// Получает размер кеша
    pub async fn get_cache_size(&self) -> usize {
        let cache = self.trace_cache.read().await;
        cache.len()
    }
}

impl Default for TraceService {
    fn default() -> Self {
        Self::new("https://tonapi.io".to_string(), None)
    }
}
