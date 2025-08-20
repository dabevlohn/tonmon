use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::subscription::manager::SubscriptionManager;
use crate::utils::error::ServiceError;
use crate::websocket::message::{ClientMessage, ServerMessage};

/// Обработчик WebSocket соединений
pub struct WebSocketHandler {
    subscription_manager: Arc<SubscriptionManager>,
}

impl WebSocketHandler {
    pub fn new(subscription_manager: Arc<SubscriptionManager>) -> Self {
        Self {
            subscription_manager,
        }
    }

    /// Обрабатывает новое WebSocket соединение
    pub async fn handle_connection(&self, socket: WebSocket) -> Result<()> {
        info!("New WebSocket connection established");

        // Создаем канал для отправки сообщений клиенту
        let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

        // Разделяем сокет на отправку и получение
        let (mut ws_sender, mut ws_receiver) = socket.split();

        // Задача для отправки сообщений клиенту
        let send_task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                match serde_json::to_string(&message) {
                    Ok(json) => {
                        if let Err(e) = ws_sender.send(Message::Text(json)).await {
                            error!("Failed to send WebSocket message: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Failed to serialize message: {}", e);
                    }
                }
            }
            debug!("WebSocket send task finished");
        });

        // Задача для получения сообщений от клиента
        let manager = Arc::clone(&self.subscription_manager);
        let client_tx = tx.clone();
        let receive_task = tokio::spawn(async move {
            let mut active_subscriptions = Vec::<String>::new();

            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Err(e) = Self::handle_text_message(
                            &text,
                            &manager,
                            &client_tx,
                            &mut active_subscriptions,
                        )
                        .await
                        {
                            error!("Error handling text message: {}", e);
                            let error_msg = ServerMessage::error(e.to_string(), None);
                            let _ = client_tx.send(error_msg);
                        }
                    }
                    Ok(Message::Binary(_)) => {
                        warn!("Received binary message, ignoring");
                    }
                    Ok(Message::Ping(data)) => {
                        debug!("Received ping, sending pong {:?}", data);
                        // Pong будет отправлен автоматически axum
                    }
                    Ok(Message::Pong(_)) => {
                        debug!("Received pong");
                    }
                    Ok(Message::Close(_)) => {
                        info!("WebSocket connection closed by client");
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                }
            }

            // Очищаем подписки при отключении
            for subscription_id in active_subscriptions {
                if let Err(e) = manager.remove_subscription(&subscription_id).await {
                    error!("Failed to remove subscription {}: {}", subscription_id, e);
                }
            }

            debug!("WebSocket receive task finished");
        });

        // Ждем завершения одной из задач
        tokio::select! {
            _ = send_task => {
                info!("WebSocket send task completed");
            }
            _ = receive_task => {
                info!("WebSocket receive task completed");
            }
        }

        Ok(())
    }

    /// Обрабатывает текстовое сообщение от клиента
    async fn handle_text_message(
        text: &str,
        manager: &Arc<SubscriptionManager>,
        sender: &mpsc::UnboundedSender<ServerMessage>,
        active_subscriptions: &mut Vec<String>,
    ) -> Result<(), ServiceError> {
        debug!("Received text message: {}", text);

        let client_message: ClientMessage =
            serde_json::from_str(text).map_err(|e| ServiceError::Serialization(e))?;

        match client_message {
            ClientMessage::Subscribe {
                address,
                subscription_id: _,
            } => {
                info!("Client subscribing to address: {}", address);

                // Создаем новую подписку
                let subscription_id = manager
                    .create_subscription(address.clone(), sender.clone())
                    .await
                    .map_err(|e| ServiceError::TonClient(e))?;

                active_subscriptions.push(subscription_id.clone());

                // Отправляем подтверждение
                let confirm_msg = ServerMessage::SubscriptionConfirm {
                    subscription_id,
                    address,
                };

                sender.send(confirm_msg).map_err(|_| {
                    ServiceError::WebSocket("Failed to send confirmation".to_string())
                })?;
            }

            ClientMessage::Unsubscribe { subscription_id } => {
                info!("Client unsubscribing from: {}", subscription_id);

                // Удаляем подписку
                manager
                    .remove_subscription(&subscription_id)
                    .await
                    .map_err(|e| ServiceError::TonClient(e))?;

                // Удаляем из активных подписок
                active_subscriptions.retain(|id| id != &subscription_id);

                // Отправляем подтверждение
                let confirm_msg = ServerMessage::UnsubscriptionConfirm { subscription_id };

                sender.send(confirm_msg).map_err(|_| {
                    ServiceError::WebSocket(
                        "Failed to send unsubscription confirmation".to_string(),
                    )
                })?;
            }

            ClientMessage::Ping => {
                debug!("Received ping from client");
                let pong_msg = ServerMessage::Pong;
                sender
                    .send(pong_msg)
                    .map_err(|_| ServiceError::WebSocket("Failed to send pong".to_string()))?;
            }
        }

        Ok(())
    }

    /// Получает статистику подключений
    pub async fn get_connection_stats(&self) -> ConnectionStats {
        let subscription_stats = self.subscription_manager.get_stats();

        ConnectionStats {
            total_subscriptions: subscription_stats.total_subscriptions,
            monitored_addresses: subscription_stats.monitored_addresses,
        }
    }
}

/// Статистика подключений
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub total_subscriptions: usize,
    pub monitored_addresses: usize,
}
