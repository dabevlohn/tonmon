use anyhow::Result;
use axum::{
    extract::{ws::WebSocketUpgrade, State},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod websocket {
    pub mod handler;
    pub mod message;
}

mod ton {
    pub mod client;
    pub mod monitor;
    pub mod trace;
}

mod subscription {
    pub mod manager;
}

mod utils {
    pub mod error;
}

use crate::subscription::manager::{MonitorEvent, SubscriptionManager};
use crate::ton::client::{TonClientConfig, TonService};
use crate::ton::monitor::TransactionMonitor;
use crate::ton::trace::TraceService;
use crate::websocket::handler::WebSocketHandler;

/// Состояние приложения
#[derive(Clone)]
pub struct AppState {
    pub websocket_handler: Arc<WebSocketHandler>,
    pub ton_service: Arc<TonService>,
    pub subscription_manager: Arc<SubscriptionManager>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Инициализация логирования
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "websocket_ton_service=info,tower_http=debug,tonlib_client=warn".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("🚀 Starting WebSocket TON Service");

    // Определяем, какая сеть используется
    let testnet = std::env::var("TON_TESTNET")
        .unwrap_or_else(|_| "false".to_string())
        .parse()
        .unwrap_or(false);

    info!(
        "🌐 Network: {}",
        if testnet { "Testnet" } else { "Mainnet" }
    );

    // Создаем TON клиент с улучшенной конфигурацией
    let ton_config = TonClientConfig {
        connection_timeout: None,
        fallback_configs: Vec::new(),
        testnet,
        api_key: std::env::var("TON_API_KEY").ok(),
        config_url: std::env::var("TON_CONFIG_URL").ok(),
    };

    info!("🔧 Initializing TON client...");
    let ton_service = match TonService::new(ton_config).await {
        Ok(service) => {
            info!("✅ TON service initialized successfully");
            Arc::new(service)
        }
        Err(e) => {
            error!("❌ Failed to create TON service: {}", e);
            error!("💡 This might be due to network connectivity issues or invalid configuration");
            error!("💡 Check your internet connection and TON network status");
            return Err(e);
        }
    };

    // Создаем сервис для получения трейсов
    let tonapi_endpoint = std::env::var("TONAPI_ENDPOINT").unwrap_or_else(|_| {
        if testnet {
            "https://testnet.tonapi.io".to_string()
        } else {
            "https://tonapi.io".to_string()
        }
    });
    let tonapi_key = std::env::var("TONAPI_KEY").ok();

    info!("🔍 TonAPI endpoint: {}", tonapi_endpoint);
    let trace_service = Arc::new(TraceService::new(tonapi_endpoint, tonapi_key));

    // Создаем каналы для событий
    let (transaction_event_tx, mut transaction_event_rx) = mpsc::unbounded_channel();
    let (monitor_event_tx, mut monitor_event_rx) = mpsc::unbounded_channel();

    // Создаем менеджер подписок
    let subscription_manager = Arc::new(SubscriptionManager::new(
        Arc::clone(&trace_service),
        monitor_event_tx,
    ));

    // Создаем монитор транзакций
    let transaction_monitor = Arc::new(TransactionMonitor::new(
        Arc::clone(&ton_service),
        Arc::clone(&trace_service),
        transaction_event_tx,
    ));

    // Создаем WebSocket хэндлер
    let websocket_handler = Arc::new(WebSocketHandler::new(Arc::clone(&subscription_manager)));

    // Создаем состояние приложения
    let app_state = AppState {
        websocket_handler: Arc::clone(&websocket_handler),
        ton_service: Arc::clone(&ton_service),
        subscription_manager: Arc::clone(&subscription_manager),
    };

    // Запускаем фоновые задачи

    // Задача для обработки событий мониторинга адресов
    let monitor_clone = Arc::clone(&transaction_monitor);
    tokio::spawn(async move {
        info!("📡 Starting address monitor event handler");
        while let Some(event) = monitor_event_rx.recv().await {
            match event {
                MonitorEvent::AddAddress(address) => {
                    debug!("➕ Adding address to monitor: {}", address);
                    if let Err(e) = monitor_clone.add_address(address.clone()).await {
                        error!("❌ Failed to add address {} to monitor: {}", address, e);
                    }
                }
                MonitorEvent::RemoveAddress(address) => {
                    debug!("➖ Removing address from monitor: {}", address);
                    if let Err(e) = monitor_clone.remove_address(&address).await {
                        error!(
                            "❌ Failed to remove address {} from monitor: {}",
                            address, e
                        );
                    }
                }
            }
        }
        warn!("📡 Address monitor event handler stopped");
    });

    // Задача для обработки событий новых транзакций
    let subscription_manager_clone = Arc::clone(&subscription_manager);
    tokio::spawn(async move {
        info!("🔄 Starting transaction event handler");
        while let Some(event) = transaction_event_rx.recv().await {
            debug!(
                "🔄 Processing transaction event for address: {}",
                event.address
            );
            if let Err(e) = subscription_manager_clone
                .handle_transaction_event(event)
                .await
            {
                error!("❌ Failed to handle transaction event: {}", e);
            }
        }
        warn!("🔄 Transaction event handler stopped");
    });

    // Задача для мониторинга транзакций
    let monitor_clone = Arc::clone(&transaction_monitor);
    tokio::spawn(async move {
        info!("👁️ Starting transaction monitor");
        if let Err(e) = monitor_clone.start().await {
            error!("❌ Transaction monitor error: {}", e);
        }
        warn!("👁️ Transaction monitor stopped");
    });

    // Создаем HTTP сервер с WebSocket поддержкой
    let app = create_router(app_state);

    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse::<u16>()
        .unwrap_or(3000);

    let addr = format!("0.0.0.0:{}", port);

    info!("🌐 Server starting on {}", addr);
    info!("📡 WebSocket endpoint: ws://localhost:{}/ws", port);
    info!("🔍 Health check: http://localhost:{}/health", port);
    info!("📊 Statistics: http://localhost:{}/stats", port);
    info!("🎯 Demo client: Open client-demo.html in browser");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Создает роутер для HTTP сервера
fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(websocket_handler))
        .route("/health", get(health_check))
        .route("/stats", get(get_stats))
        .route("/api/validate-address", post(validate_address))
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive()),
        )
        .with_state(state)
}

/// Главная страница с документацией
async fn index() -> Html<&'static str> {
    Html(
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>WebSocket TON Service</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 40px; }
        .code { background: #f4f4f4; padding: 10px; margin: 10px 0; }
        .endpoint { color: #0066cc; font-weight: bold; }
        .status { padding: 10px; border-radius: 4px; margin: 10px 0; }
        .status.success { background-color: #d4edda; color: #155724; }
        .status.warning { background-color: #fff3cd; color: #856404; }
    </style>
</head>
<body>
    <h1>🚀 WebSocket TON Service</h1>
    <p>Высокопроизводительный сервис для мониторинга транзакций в сети TON через WebSocket соединения.</p>

    <div class="status success">
        ✅ Сервис запущен и готов к работе
    </div>

    <h2>📡 WebSocket API</h2>
    <p><span class="endpoint">ws://localhost:3000/ws</span></p>

    <h3>Сообщения клиента:</h3>
    <div class="code">
    // Подписка на адрес<br/>
    {"type": "Subscribe", "address": "EQD3o5h_..."}<br/><br/>

    // Отписка<br/>
    {"type": "Unsubscribe", "subscription_id": "uuid"}<br/><br/>

    // Пинг<br/>
    {"type": "Ping"}
    </div>

    <h3>Сообщения сервера:</h3>
    <div class="code">
    // Подтверждение подписки<br/>
    {"type": "SubscriptionConfirm", "subscription_id": "uuid", "address": "..."}<br/><br/>

    // Новый трейс транзакции<br/>
    {"type": "TransactionTrace", "subscription_id": "uuid", "address": "...", "trace": {...}}<br/><br/>

    // Ошибка<br/>
    {"type": "Error", "message": "...", "subscription_id": "..."}
    </div>

    <h2>🌐 HTTP API</h2>
    <p><span class="endpoint">GET /health</span> - проверка здоровья сервиса</p>
    <p><span class="endpoint">GET /stats</span> - статистика подписок</p>
    <p><span class="endpoint">POST /api/validate-address</span> - валидация TON адреса</p>

    <h2>🎯 Демо клиент</h2>
    <p>Откройте файл <strong>client-demo.html</strong> в браузере для интерактивного тестирования.</p>

    <h2>⚠️ Решение проблем</h2>
    <div class="status warning">
        <strong>Unknown code hash:</strong> Если в логах появляется эта ошибка, это означает, 
        что адрес не существует или имеет неизвестный тип смарт-контракта. 
        Сервис автоматически обрабатывает эту ситуацию.
    </div>
</body>
</html>
    "#,
    )
}

/// WebSocket хэндлер
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = state.websocket_handler.handle_connection(socket).await {
            error!("❌ WebSocket connection error: {}", e);
        }
    })
}

/// Проверка здоровья сервиса
async fn health_check(State(state): State<AppState>) -> Json<Value> {
    let (ton_connected, last_block_seqno, error_msg) =
        match state.ton_service.get_last_block().await {
            Ok(block) => (true, Some(block.seqno), None),
            Err(e) => (false, None, Some(e.to_string())),
        };

    Json(json!({
        "status": if ton_connected { "healthy" } else { "degraded" },
        "ton_connected": ton_connected,
        "last_block_seqno": last_block_seqno,
        "error": error_msg,
        "subscriptions": state.subscription_manager.get_subscription_count(),
        "monitored_addresses": state.subscription_manager.get_monitored_addresses_count(),
        "timestamp": chrono::Utc::now().timestamp()
    }))
}

/// Получение статистики
async fn get_stats(State(state): State<AppState>) -> Json<Value> {
    let stats = state.subscription_manager.get_stats();

    Json(json!({
        "subscriptions": {
            "total": stats.total_subscriptions,
            "monitored_addresses": stats.monitored_addresses,
            "addresses": stats.addresses
        },
        "service_status": "running",
        "timestamp": chrono::Utc::now().timestamp()
    }))
}

/// Валидация TON адреса с улучшенной обработкой
async fn validate_address(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let address = match payload.get("address").and_then(|v| v.as_str()) {
        Some(addr) => addr,
        None => {
            return Json(json!({
                "valid": false,
                "error": "Missing address field"
            }));
        }
    };

    // Проверяем формат адреса
    match state.ton_service.validate_address(address) {
        Ok(_) => {
            // Дополнительно проверяем, что адрес активен
            match state.ton_service.is_address_active(address).await {
                Ok(is_active) => Json(json!({
                    "valid": true,
                    "address": address,
                    "active": is_active,
                    "note": if !is_active {
                        Some("Address format is valid but account may not exist or have unknown contract type")
                    } else {
                        None
                    }
                })),
                Err(e) => Json(json!({
                    "valid": true,
                    "address": address,
                    "active": false,
                    "error": format!("Could not check account status: {}", e)
                })),
            }
        }
        Err(e) => Json(json!({
            "valid": false,
            "error": e.to_string()
        })),
    }
}
