use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration, Instant};
use tracing::{debug, error, info, warn};

use super::client::TonService;
use super::trace::TraceService;
use crate::websocket::message::TransactionTrace;

/// Событие о новой транзакции
#[derive(Debug, Clone)]
pub struct TransactionEvent {
    pub address: String,
    pub transaction_hash: String,
    pub logical_time: u64,
    pub timestamp: u32,
}

/// Информация о мониторинге адреса
#[derive(Debug, Clone)]
struct AddressMonitor {
    pub address: String,
    pub last_lt: i64, // Изменил на i64 согласно API tonlib-rs
    pub last_check: Instant,
    pub subscription_count: u32,
}

/// Сервис для мониторинга транзакций TON
pub struct TransactionMonitor {
    ton_service: Arc<TonService>,
    trace_service: Arc<TraceService>,
    monitored_addresses: Arc<RwLock<HashMap<String, AddressMonitor>>>,
    event_sender: mpsc::UnboundedSender<TransactionEvent>,
    polling_interval: Duration,
}

impl TransactionMonitor {
    pub fn new(
        ton_service: Arc<TonService>,
        trace_service: Arc<TraceService>,
        event_sender: mpsc::UnboundedSender<TransactionEvent>,
    ) -> Self {
        Self {
            ton_service,
            trace_service,
            monitored_addresses: Arc::new(RwLock::new(HashMap::new())),
            event_sender,
            polling_interval: Duration::from_secs(5), // Проверяем каждые 5 секунд
        }
    }

    /// Запускает мониторинг транзакций
    pub async fn start(&self) -> Result<()> {
        info!("Starting transaction monitor");

        let mut ticker = interval(self.polling_interval);

        loop {
            ticker.tick().await;

            if let Err(e) = self.check_transactions().await {
                error!("Error checking transactions: {}", e);
            }
        }
    }

    /// Добавляет адрес для мониторинга
    pub async fn add_address(&self, address: String) -> Result<()> {
        // Проверяем валидность адреса
        self.ton_service.validate_address(&address)?;

        let mut addresses = self.monitored_addresses.write().await;

        match addresses.get_mut(&address) {
            Some(monitor) => {
                monitor.subscription_count += 1;
                info!(
                    "Increased subscription count for address {} to {}",
                    address, monitor.subscription_count
                );
            }
            None => {
                // Получаем последнюю логическую временную метку для адреса
                let last_lt = self.get_last_lt_for_address(&address).await.unwrap_or(0);

                let monitor = AddressMonitor {
                    address: address.clone(),
                    last_lt,
                    last_check: Instant::now(),
                    subscription_count: 1,
                };

                addresses.insert(address.clone(), monitor);
                info!(
                    "Added new address for monitoring: {} (starting from lt: {})",
                    address, last_lt
                );
            }
        }

        Ok(())
    }

    /// Убирает адрес из мониторинга
    pub async fn remove_address(&self, address: &str) -> Result<bool> {
        let mut addresses = self.monitored_addresses.write().await;

        match addresses.get_mut(address) {
            Some(monitor) => {
                monitor.subscription_count = monitor.subscription_count.saturating_sub(1);

                if monitor.subscription_count == 0 {
                    addresses.remove(address);
                    info!("Removed address from monitoring: {}", address);
                    Ok(true)
                } else {
                    info!(
                        "Decreased subscription count for address {} to {}",
                        address, monitor.subscription_count
                    );
                    Ok(false)
                }
            }
            None => {
                warn!("Attempted to remove non-monitored address: {}", address);
                Ok(false)
            }
        }
    }

    /// Получает список мониторинговых адресов
    pub async fn get_monitored_addresses(&self) -> Vec<String> {
        let addresses = self.monitored_addresses.read().await;
        addresses.keys().cloned().collect()
    }

    /// Проверяет новые транзакции для всех мониторинговых адресов
    async fn check_transactions(&self) -> Result<()> {
        let addresses = {
            let addresses_guard = self.monitored_addresses.read().await;
            addresses_guard.clone()
        };

        debug!("Checking transactions for {} addresses", addresses.len());

        if addresses.is_empty() {
            debug!("Empty addresses");
            return Ok(());
        }

        for (address, monitor) in addresses {
            if let Err(e) = self.check_address_transactions(&address, &monitor).await {
                error!("Error checking transactions for address {}: {}", address, e);
            }
        }

        Ok(())
    }

    /// Проверяет новые транзакции для конкретного адреса
    async fn check_address_transactions(
        &self,
        address: &str,
        monitor: &AddressMonitor,
    ) -> Result<()> {
        // Преобразуем last_lt в u64 для совместимости с существующим API
        let from_lt = if monitor.last_lt > 0 {
            Some(monitor.last_lt as u64)
        } else {
            None
        };

        let transactions = self
            .ton_service
            .get_transactions(address, from_lt, 15)
            .await?;

        let mut new_transactions = Vec::new();
        let mut max_lt = monitor.last_lt;

        // Фильтруем новые транзакции
        for tx in transactions {
            // Получаем logical time из transaction_id
            let tx_lt = tx.transaction_id.lt;

            if tx_lt > monitor.last_lt {
                new_transactions.push(tx.clone());
                max_lt = max_lt.max(tx_lt);
            }
        }

        if !new_transactions.is_empty() {
            info!(
                "Found {} new transactions for address {}",
                new_transactions.len(),
                address
            );

            // Обновляем последний lt
            {
                let mut addresses = self.monitored_addresses.write().await;
                if let Some(monitor) = addresses.get_mut(address) {
                    monitor.last_lt = max_lt;
                    monitor.last_check = Instant::now();
                }
            }

            // Отправляем события о новых транзакциях
            for tx in new_transactions {
                let event = TransactionEvent {
                    address: address.to_string(),
                    transaction_hash: hex::encode(&tx.transaction_id.hash), // Преобразуем в hex строку
                    logical_time: tx.transaction_id.lt as u64,
                    timestamp: tx.utime as u32, // Используем utime вместо now
                };

                if let Err(e) = self.event_sender.send(event) {
                    error!("Failed to send transaction event: {}", e);
                }
            }
        }

        Ok(())
    }

    /// Получает последнюю логическую временную метку для адреса
    async fn get_last_lt_for_address(&self, address: &str) -> Result<i64> {
        let transactions = self.ton_service.get_transactions(address, None, 1).await?;

        Ok(transactions
            .first()
            .map(|tx| tx.transaction_id.lt)
            .unwrap_or(0))
    }

    /// Получает количество мониторинговых адресов
    pub async fn get_monitor_count(&self) -> usize {
        let addresses = self.monitored_addresses.read().await;
        addresses.len()
    }
}
