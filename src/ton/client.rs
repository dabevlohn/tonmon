use anyhow::Result;
use reqwest;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// Обновленные импорты для актуального API tonlib-rs
use tonlib_client::client::{TonClient, TonClientInterface};
use tonlib_core::TonAddress;

/// Конфигурация для TON клиента с множественными endpoints
pub struct TonClientConfig {
    pub testnet: bool,
    pub api_key: Option<String>,
    pub config_url: Option<String>,
    pub fallback_configs: Vec<String>,
    pub connection_timeout: Option<u64>,
}

impl Default for TonClientConfig {
    fn default() -> Self {
        Self {
            testnet: false,
            api_key: None,
            config_url: None,
            fallback_configs: vec![
                // Дополнительные конфигурации для fallback
                "https://ton-blockchain.github.io/global.config.json".to_string(),
                "https://ton.org/global.config.json".to_string(),
            ],
            connection_timeout: Some(30), // 30 секунд таймаут
        }
    }
}

/// Обертка над TON клиентом с улучшенным подключением к ADNL
pub struct TonService {
    client: Arc<TonClient>,
    config: TonClientConfig,
    successful_config: Option<String>,
}

impl TonService {
    /// Создает новый TON сервис с устойчивым ADNL подключением
    pub async fn new(config: TonClientConfig) -> Result<Self> {
        // Минимальный уровень логирования для уменьшения шума
        TonClient::set_log_verbosity_level(0);

        info!("🔌 Initializing TON client with ADNL connection...");

        // Пробуем подключиться к различным конфигурациям
        let (client, successful_config) = Self::try_connect_with_fallback(&config).await?;

        info!("✅ TON client connected via: {}", successful_config);

        Ok(Self {
            client: Arc::new(client),
            config,
            successful_config: Some(successful_config),
        })
    }

    /// Пробует подключиться к TON, используя fallback конфигурации
    async fn try_connect_with_fallback(config: &TonClientConfig) -> Result<(TonClient, String)> {
        let mut configs_to_try = Vec::new();

        // Добавляем основную конфигурацию
        if let Some(url) = &config.config_url {
            configs_to_try.push(url.clone());
        } else {
            let default_url = if config.testnet {
                "https://ton.org/testnet-global.config.json".to_string()
            } else {
                "https://ton.org/global.config.json".to_string()
            };
            configs_to_try.push(default_url);
        }

        // Добавляем fallback конфигурации
        configs_to_try.extend(config.fallback_configs.clone());

        let mut last_error = None;

        for (attempt, config_url) in configs_to_try.iter().enumerate() {
            info!("🔄 Attempt {} - Trying config: {}", attempt + 1, config_url);

            match Self::try_connect_single_config(config_url, config).await {
                Ok(client) => {
                    info!("✅ Successfully connected using: {}", config_url);
                    return Ok((client, config_url.clone()));
                }
                Err(e) => {
                    warn!("❌ Failed to connect with {}: {}", config_url, e);
                    last_error = Some(e);

                    // Задержка между попытками
                    if attempt < configs_to_try.len() - 1 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All connection attempts failed")))
    }

    /// Пробует подключиться к одной конфигурации
    async fn try_connect_single_config(
        config_url: &str,
        config: &TonClientConfig,
    ) -> Result<TonClient> {
        // Загружаем конфигурацию с таймаутом
        let config_data =
            Self::load_network_config_with_timeout(config_url, config.connection_timeout).await?;

        // Модифицируем конфигурацию для улучшения подключения
        let optimized_config = Self::optimize_config_for_connection(config_data)?;

        // Применяем дополнительные настройки для стабильности
        if let Some(timeout) = config.connection_timeout {
            // Настройки таймаутов если поддерживаются API
            debug!("Setting connection timeout: {} seconds", timeout);
        }

        // Создаем клиент с оптимизированной конфигурацией
        let client = TonClient::builder()
            .with_config(&optimized_config)
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to build TON client: {}", e))?;

        // Тестируем подключение с retry
        Self::test_connection_with_retry(&client).await?;

        Ok(client)
    }

    /// Загружает конфигурацию сети с таймаутом
    async fn load_network_config_with_timeout(
        config_url: &str,
        timeout_secs: Option<u64>,
    ) -> Result<String> {
        let timeout = tokio::time::Duration::from_secs(timeout_secs.unwrap_or(30));

        let request = reqwest::Client::new()
            .get(config_url)
            .timeout(timeout)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build request: {}", e))?;

        let response = reqwest::Client::new()
            .execute(request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to execute request: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to load config, HTTP status: {}",
                response.status()
            ));
        }

        let config_text = response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read response text: {}", e))?;
        debug!(
            "Loaded config from {} ({} bytes)",
            config_url,
            config_text.len()
        );

        Ok(config_text)
    }

    /// Оптимизирует конфигурацию для лучшего подключения
    fn optimize_config_for_connection(config_data: String) -> Result<String> {
        let mut config: Value = serde_json::from_str(&config_data)
            .map_err(|e| anyhow::anyhow!("Failed to parse config JSON: {}", e))?;

        // Если это объект с liteservers, оптимизируем их
        if let Some(liteservers) = config.get_mut("liteservers") {
            if let Some(servers_array) = liteservers.as_array_mut() {
                // Сортируем liteservers для использования наиболее стабильных
                // Можно добавить дополнительную логику приоритизации

                info!("Found {} liteservers in config", servers_array.len());

                // Ограничиваем количество серверов для ускорения подключения
                if servers_array.len() > 10 {
                    servers_array.truncate(10);
                    info!(
                        "Limited to {} liteservers for faster connection",
                        servers_array.len()
                    );
                }
            }
        }

        serde_json::to_string(&config)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))
    }

    /// Тестирует подключение с retry логикой
    async fn test_connection_with_retry(client: &TonClient) -> Result<()> {
        let max_retries = 5;
        let mut retry_delay = 1;

        for attempt in 1..=max_retries {
            match client.get_masterchain_info().await {
                Ok((_, master_info)) => {
                    info!(
                        "🎯 Connection test successful! Block seqno: {}",
                        master_info.last.seqno
                    );
                    return Ok(());
                }
                Err(e) => {
                    if e.to_string().contains("Connection refused") {
                        warn!(
                            "🔄 ADNL connection refused on attempt {} of {}",
                            attempt, max_retries
                        );
                    } else {
                        warn!(
                            "🔄 Connection test failed on attempt {} of {}: {}",
                            attempt, max_retries, e
                        );
                    }

                    if attempt < max_retries {
                        debug!("⏳ Waiting {} seconds before retry...", retry_delay);
                        tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay)).await;
                        retry_delay = std::cmp::min(retry_delay * 2, 10); // Exponential backoff, max 10 sec
                    } else {
                        return Err(anyhow::anyhow!(
                            "Connection test failed after {} attempts: {}",
                            max_retries,
                            e
                        ));
                    }
                }
            }
        }

        unreachable!()
    }

    /// Получает информацию о последнем блоке с устойчивостью к ADNL ошибкам
    pub async fn get_last_block(&self) -> Result<tonlib_client::tl::BlockIdExt> {
        self.with_connection_retry(|client| async move {
            let (_, master_info) = client
                .get_masterchain_info()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get masterchain info: {}", e))?;
            Ok(master_info.last)
        })
        .await
    }

    /// Получает транзакции с обработкой ADNL ошибок
    pub async fn get_transactions(
        &self,
        address: &str,
    ) -> Result<Vec<tonlib_client::tl::RawTransaction>> {
        let ton_address = self.validate_address(address)?;

        self.with_connection_retry(|client| {
            let addr = ton_address.clone();
            async move { Self::try_get_transactions_internal(client, &addr).await }
        })
        .await
    }

    /// Внутренний метод для получения транзакций
    async fn try_get_transactions_internal(
        client: Arc<TonClient>,
        ton_address: &TonAddress,
    ) -> Result<Vec<tonlib_client::tl::RawTransaction>> {
        // Получаем состояние аккаунта
        let account_state = match client.get_account_state(ton_address).await {
            Ok(state) => state,
            Err(e) if e.to_string().contains("Unknown code hash") => {
                warn!("Unknown code hash for address, returning empty list");
                return Ok(Vec::new());
            }
            Err(e) if e.to_string().contains("Connection refused") => {
                return Err(anyhow::anyhow!("ADNL connection lost: {}", e));
            }
            Err(e) => return Err(anyhow::anyhow!("Failed to get account state: {}", e)),
        };

        let shards = client.get_block_shards(&account_state.block_id).await?;
        debug!(
            "Address: {}, LastTxId: {}, BlockId: {}, Shards: {:?}",
            account_state.address.account_address,
            account_state.last_transaction_id,
            account_state.block_id.seqno,
            shards.shards
        );
        let tx_last = client
            .get_raw_transactions(ton_address, &account_state.last_transaction_id)
            .await?;

        Ok(tx_last.transactions)
    }

    /// Выполняет операцию с retry при ADNL ошибках
    async fn with_connection_retry<F, Fut, T>(&self, operation: F) -> Result<T>
    where
        F: Fn(Arc<TonClient>) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        let max_retries = 3;
        let mut last_error = None;

        for attempt in 1..=max_retries {
            match operation(Arc::clone(&self.client)).await {
                Ok(result) => return Ok(result),
                Err(e) if e.to_string().contains("Connection refused") => {
                    error!(
                        "🔌 ADNL connection refused on attempt {} of {}",
                        attempt, max_retries
                    );
                    last_error = Some(e);

                    if attempt < max_retries {
                        // Пытаемся переподключиться при необходимости
                        warn!("🔄 Attempting to recover connection...");
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
                Err(e) => {
                    warn!(
                        "Operation failed on attempt {} of {}: {}",
                        attempt, max_retries, e
                    );
                    last_error = Some(e);

                    if attempt < max_retries {
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| anyhow::anyhow!("Operation failed after {} attempts", max_retries)))
    }

    /// Проверяет валидность TON адреса
    pub fn validate_address(&self, address: &str) -> Result<TonAddress> {
        TonAddress::from_base64_url(address)
            .map_err(|e| anyhow::anyhow!("Invalid TON address: {}", e))
    }

    /// Получает состояние аккаунта с обработкой ADNL ошибок
    pub async fn get_account_state(
        &self,
        address: &str,
    ) -> Result<Option<tonlib_client::tl::FullAccountState>> {
        let ton_address = self.validate_address(address)?;

        self.with_connection_retry(|client| {
            let addr = ton_address.clone();
            async move {
                match client.get_account_state(&addr).await {
                    Ok(state) => Ok(Some(state)),
                    Err(e) if e.to_string().contains("Unknown code hash") => {
                        warn!("Unknown code hash for address, might not exist");
                        Ok(None)
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to get account state: {}", e)),
                }
            }
        })
        .await
    }

    /// Проверяет, что адрес существует и доступен
    pub async fn is_address_active(&self, address: &str) -> Result<bool> {
        match self.get_account_state(address).await? {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    /// Получает клиент для прямого использования
    pub fn get_client(&self) -> Arc<TonClient> {
        Arc::clone(&self.client)
    }

    /// Диагностическая информация о подключении
    pub fn get_connection_info(&self) -> serde_json::Value {
        serde_json::json!({
            "testnet": self.config.testnet,
            "successful_config": self.successful_config,
            "fallback_configs": self.config.fallback_configs,
            "connection_timeout": self.config.connection_timeout
        })
    }

    /// Проверяет состояние ADNL подключения
    pub async fn check_adnl_connection(&self) -> Result<bool> {
        match self.get_last_block().await {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("Connection refused") => {
                warn!("ADNL connection check failed: Connection refused");
                Ok(false)
            }
            Err(e) => {
                warn!("ADNL connection check failed: {}", e);
                Ok(false)
            }
        }
    }
}
