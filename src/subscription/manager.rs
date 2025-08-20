use anyhow::Result;
use dashmap::DashMap;
use serde::Serialize; // Добавляем импорт Serialize
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::ton::monitor::TransactionEvent;
use crate::ton::trace::TraceService;
use crate::websocket::message::ServerMessage;

/// Информация о подписке
#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: String,
    pub address: String,
    pub sender: mpsc::UnboundedSender<ServerMessage>,
}

/// Менеджер подписок на WebSocket соединения
pub struct SubscriptionManager {
    // Подписки по ID
    subscriptions: DashMap<String, Subscription>,
    // Подписки по адресу - может быть несколько подписок на один адрес
    address_subscriptions: DashMap<String, Vec<String>>,
    trace_service: Arc<TraceService>,
    // Канал для отправки событий мониторинга адресов
    monitor_event_sender: mpsc::UnboundedSender<MonitorEvent>,
}

/// События для управления мониторингом адресов
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    AddAddress(String),
    RemoveAddress(String),
}

impl SubscriptionManager {
    pub fn new(
        trace_service: Arc<TraceService>,
        monitor_event_sender: mpsc::UnboundedSender<MonitorEvent>,
    ) -> Self {
        Self {
            subscriptions: DashMap::new(),
            address_subscriptions: DashMap::new(),
            trace_service,
            monitor_event_sender,
        }
    }

    /// Создает новую подписку
    pub async fn create_subscription(
        &self,
        address: String,
        sender: mpsc::UnboundedSender<ServerMessage>,
    ) -> Result<String> {
        let subscription_id = Uuid::new_v4().to_string();

        let subscription = Subscription {
            id: subscription_id.clone(),
            address: address.clone(),
            sender,
        };

        // Добавляем подписку
        self.subscriptions
            .insert(subscription_id.clone(), subscription);

        // Добавляем в индекс по адресам
        self.address_subscriptions
            .entry(address.clone())
            .or_default()
            .push(subscription_id.clone());

        // Отправляем событие для добавления адреса в мониторинг
        if let Err(e) = self
            .monitor_event_sender
            .send(MonitorEvent::AddAddress(address.clone()))
        {
            error!("Failed to send monitor event: {}", e);
        }

        info!(
            "Created subscription {} for address {}",
            subscription_id, address
        );
        Ok(subscription_id)
    }

    /// Удаляет подписку
    pub async fn remove_subscription(&self, subscription_id: &str) -> Result<()> {
        if let Some((_, subscription)) = self.subscriptions.remove(subscription_id) {
            let address = subscription.address.clone();

            // Удаляем из индекса по адресам
            if let Some(mut subs) = self.address_subscriptions.get_mut(&address) {
                subs.retain(|id| id != subscription_id);

                // Если больше нет подписок на этот адрес, убираем адрес из мониторинга
                if subs.is_empty() {
                    self.address_subscriptions.remove(&address);

                    if let Err(e) = self
                        .monitor_event_sender
                        .send(MonitorEvent::RemoveAddress(address.clone()))
                    {
                        error!("Failed to send monitor remove event: {}", e);
                    }
                }
            }

            info!(
                "Removed subscription {} for address {}",
                subscription_id, address
            );
        } else {
            warn!(
                "Attempted to remove non-existent subscription: {}",
                subscription_id
            );
        }

        Ok(())
    }

    /// Обрабатывает событие новой транзакции
    pub async fn handle_transaction_event(&self, event: TransactionEvent) -> Result<()> {
        debug!("Handling transaction event for address: {}", event.address);

        // Получаем все подписки для данного адреса
        let subscription_ids = if let Some(ids) = self.address_subscriptions.get(&event.address) {
            ids.clone()
        } else {
            debug!("No subscriptions found for address: {}", event.address);
            return Ok(());
        };

        // Получаем трейс транзакции
        let trace = match self
            .trace_service
            .get_transaction_trace(&event.transaction_hash)
            .await
        {
            Ok(trace) => trace,
            Err(e) => {
                error!(
                    "Failed to get trace for transaction {}: {}",
                    event.transaction_hash, e
                );
                return Err(e);
            }
        };

        info!(
            "Got trace for transaction {} with {} children",
            event.transaction_hash,
            trace.children.len()
        );

        // Отправляем трейс всем подписчикам
        let mut failed_subscriptions = Vec::new();

        for subscription_id in subscription_ids {
            if let Some(subscription) = self.subscriptions.get(&subscription_id) {
                let message = ServerMessage::TransactionTrace {
                    subscription_id: subscription.id.clone(),
                    address: event.address.clone(),
                    trace: Box::new(trace.clone()),
                };

                if let Err(_) = subscription.sender.send(message) {
                    // Соединение закрыто, помечаем подписку для удаления
                    failed_subscriptions.push(subscription_id);
                }
            }
        }

        // Удаляем неработающие подписки
        for failed_id in failed_subscriptions {
            if let Err(e) = self.remove_subscription(&failed_id).await {
                error!("Failed to remove failed subscription {}: {}", failed_id, e);
            }
        }

        Ok(())
    }

    /// Отправляет сообщение конкретной подписке
    pub async fn send_to_subscription(
        &self,
        subscription_id: &str,
        message: ServerMessage,
    ) -> Result<()> {
        if let Some(subscription) = self.subscriptions.get(subscription_id) {
            if let Err(_) = subscription.sender.send(message) {
                // Соединение закрыто, удаляем подписку
                self.remove_subscription(subscription_id).await?;
                return Err(anyhow::anyhow!("Subscription connection closed"));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Subscription not found: {}",
                subscription_id
            ));
        }

        Ok(())
    }

    /// Получает количество активных подписок
    pub fn get_subscription_count(&self) -> usize {
        self.subscriptions.len()
    }

    /// Получает количество мониторинговых адресов
    pub fn get_monitored_addresses_count(&self) -> usize {
        self.address_subscriptions.len()
    }

    /// Получает статистику подписок
    pub fn get_stats(&self) -> SubscriptionStats {
        SubscriptionStats {
            total_subscriptions: self.subscriptions.len(),
            monitored_addresses: self.address_subscriptions.len(),
            addresses: self
                .address_subscriptions
                .iter()
                .map(|entry| AddressStats {
                    address: entry.key().clone(),
                    subscription_count: entry.value().len(),
                })
                .collect(),
        }
    }

    /// Очищает все подписки (для graceful shutdown)
    pub async fn clear_all(&self) {
        info!("Clearing all subscriptions");

        for entry in self.subscriptions.iter() {
            let _ = entry.sender.send(ServerMessage::Error {
                message: "Service shutting down".to_string(),
                subscription_id: Some(entry.id.clone()),
            });
        }

        self.subscriptions.clear();
        self.address_subscriptions.clear();
    }
}

/// Статистика подписок
#[derive(Debug, Clone, Serialize)] // Добавляем Serialize
pub struct SubscriptionStats {
    pub total_subscriptions: usize,
    pub monitored_addresses: usize,
    pub addresses: Vec<AddressStats>,
}

/// Статистика по адресу
#[derive(Debug, Clone, Serialize)] // Добавляем Serialize
pub struct AddressStats {
    pub address: String,
    pub subscription_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_subscription_creation() {
        let (monitor_tx, _monitor_rx) = mpsc::unbounded_channel();
        let trace_service = Arc::new(TraceService::default());

        let manager = SubscriptionManager::new(trace_service, monitor_tx);

        let (sub_tx, _sub_rx) = mpsc::unbounded_channel();
        let result = manager
            .create_subscription(
                "EQD3o5h_LmFwcSXvZWuOy9W9y7cE3I4n2Ni0kxqTNPhjj5yt".to_string(),
                sub_tx,
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(manager.get_subscription_count(), 1);
    }

    #[tokio::test]
    async fn test_subscription_removal() {
        let (monitor_tx, _monitor_rx) = mpsc::unbounded_channel();
        let trace_service = Arc::new(TraceService::default());
        let manager = SubscriptionManager::new(trace_service, monitor_tx);

        let (sub_tx, _sub_rx) = mpsc::unbounded_channel();
        let subscription_id = manager
            .create_subscription(
                "EQD3o5h_LmFwcSXvZWuOy9W9y7cE3I4n2Ni0kxqTNPhjj5yt".to_string(),
                sub_tx,
            )
            .await
            .unwrap();

        assert_eq!(manager.get_subscription_count(), 1);

        manager.remove_subscription(&subscription_id).await.unwrap();
        assert_eq!(manager.get_subscription_count(), 0);
    }
}
