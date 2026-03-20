//! Lightweight WSS client for Polymarket market channel updates.
//!
//! This module focuses on the public market channel exposed at
//! `wss://ws-subscriptions-clob.polymarket.com/ws/`. It maintains a single
//! reconnecting connection, replays the most recent market/asset subscriptions,
//! and exposes typed events for books, price changes, tick size changes, and
//! last trade notifications.

use crate::errors::{PolyError, Result};
use crate::types::{ApiCredentials, OrderSummary, Side};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};
use tracing::warn;

const DEFAULT_WSS_BASE: &str = "wss://ws-subscriptions-clob.polymarket.com";
const MARKET_CHANNEL_PATH: &str = "/ws/market";
const USER_CHANNEL_PATH: &str = "/ws/user";
const BASE_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(10);
const MAX_RECONNECT_ATTEMPTS: u32 = 8;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Represents a parsed market broadcast from the public market channel.
#[derive(Debug, Clone)]
pub enum WssMarketEvent {
    Book(MarketBook),
    PriceChange(PriceChangeMessage),
    TickSizeChange(TickSizeChangeMessage),
    LastTrade(LastTradeMessage),
    BestBidAsk(BestBidAskMessage),
    NewMarket(NewMarketMessage),
    MarketResolved(MarketResolvedMessage),
}

/// Events emitted by the authenticated user channel.
#[derive(Debug, Clone)]
pub enum WssUserEvent {
    Trade(WssUserTradeMessage),
    Order(WssUserOrderMessage),
}

/// Trade notifications scoped to the authenticated user.
#[derive(Debug, Clone, Deserialize)]
pub struct WssUserTradeMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    pub asset_id: String,
    pub id: String,
    pub last_update: String,
    #[serde(default)]
    pub maker_orders: Vec<MakerOrder>,
    pub market: String,
    #[serde(default)]
    pub matchtime: String,
    pub outcome: String,
    pub owner: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: rust_decimal::Decimal,
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: rust_decimal::Decimal,
    pub status: String,
    pub taker_order_id: String,
    pub timestamp: String,
    pub trade_owner: String,
    #[serde(rename = "type")]
    pub message_type: String,
}

/// Maker order details included in user trade events.
#[derive(Debug, Clone, Deserialize)]
pub struct MakerOrder {
    pub asset_id: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub matched_amount: rust_decimal::Decimal,
    pub order_id: String,
    pub outcome: String,
    pub owner: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: rust_decimal::Decimal,
}

/// Order notifications scoped to the authenticated user.
#[derive(Debug, Clone, Deserialize)]
pub struct WssUserOrderMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    #[serde(default)]
    pub associate_trades: Option<Vec<String>>,
    pub asset_id: String,
    pub id: String,
    pub market: String,
    pub order_owner: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub original_size: rust_decimal::Decimal,
    pub outcome: String,
    pub owner: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: rust_decimal::Decimal,
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub size_matched: rust_decimal::Decimal,
    pub timestamp: String,
    #[serde(rename = "type")]
    pub message_type: String,
}

/// Book summary message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketBook {
    #[serde(rename = "event_type")]
    pub event_type: String,
    pub asset_id: String,
    pub market: String,
    pub timestamp: String,
    pub hash: String,
    pub bids: Vec<OrderSummary>,
    pub asks: Vec<OrderSummary>,
}

/// Payload for price change notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChangeMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    pub market: String,
    #[serde(rename = "price_changes")]
    pub price_changes: Vec<PriceChangeEntry>,
    pub timestamp: String,
}

/// Individual price change entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChangeEntry {
    pub asset_id: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: rust_decimal::Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: rust_decimal::Decimal,
    pub side: Side,
    pub hash: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub best_bid: rust_decimal::Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub best_ask: rust_decimal::Decimal,
}

/// Tick size change events.
#[derive(Debug, Clone, Deserialize)]
pub struct TickSizeChangeMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    pub asset_id: String,
    pub market: String,
    #[serde(rename = "old_tick_size", with = "rust_decimal::serde::str")]
    pub old_tick_size: rust_decimal::Decimal,
    #[serde(rename = "new_tick_size", with = "rust_decimal::serde::str")]
    pub new_tick_size: rust_decimal::Decimal,
    pub timestamp: String,
}

/// Trade events emitted when a trade settles.
#[derive(Debug, Clone, Deserialize)]
pub struct LastTradeMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    pub asset_id: String,
    pub fee_rate_bps: String,
    pub market: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: rust_decimal::Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: rust_decimal::Decimal,
    pub side: Side,
    pub timestamp: String,
}

/// Best bid/ask updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BestBidAskMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    pub market: String,
    pub asset_id: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub best_bid: rust_decimal::Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub best_ask: rust_decimal::Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub spread: rust_decimal::Decimal,
    pub timestamp: String,
}

/// Event metadata nested inside market lifecycle messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketLifecycleEventMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub ticker: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
}

/// New market lifecycle notifications.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NewMarketMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub market: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub assets_ids: Vec<String>,
    #[serde(default)]
    pub outcomes: Vec<String>,
    #[serde(default)]
    pub event_message: Option<MarketLifecycleEventMessage>,
    #[serde(default)]
    pub timestamp: String,
}

/// Market resolved lifecycle notifications.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketResolvedMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub market: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub assets_ids: Vec<String>,
    #[serde(default)]
    pub outcomes: Vec<String>,
    #[serde(default)]
    pub winning_asset_id: String,
    #[serde(default)]
    pub winning_outcome: String,
    #[serde(default)]
    pub event_message: Option<MarketLifecycleEventMessage>,
    #[serde(default)]
    pub timestamp: String,
}

/// Simple stats for monitoring connection health.
#[derive(Debug, Clone, Default)]
pub struct WssStats {
    pub messages_received: u64,
    pub errors: u64,
    pub reconnect_count: u32,
    pub last_message_time: Option<DateTime<Utc>>,
}

/// Reconnecting client for the market channel.
pub struct WssMarketClient {
    connect_url: String,
    connection: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    subscribed_asset_ids: Vec<String>,
    stats: WssStats,
    disconnect_history: VecDeque<DateTime<Utc>>,
    pending_events: VecDeque<WssMarketEvent>,
}

impl Default for WssMarketClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WssMarketClient {
    /// Create a new instance using the default Polymarket WSS base.
    pub fn new() -> Self {
        Self::with_url(DEFAULT_WSS_BASE)
    }

    /// Create a new client against a custom endpoint (useful for tests).
    pub fn with_url(url: &str) -> Self {
        let trimmed = url.trim_end_matches('/');
        let connect_url = format!("{}{}", trimmed, MARKET_CHANNEL_PATH);
        Self {
            connection: None,
            subscribed_asset_ids: Vec::new(),
            stats: WssStats::default(),
            disconnect_history: VecDeque::with_capacity(5),
            connect_url,
            pending_events: VecDeque::new(),
        }
    }

    /// Access connection stats for observability.
    pub fn stats(&self) -> WssStats {
        self.stats.clone()
    }

    fn format_subscription(&self) -> Value {
        json!({
            "type": "market",
            "assets_ids": self.subscribed_asset_ids,
            "custom_feature_enabled": true,
        })
    }

    fn format_subscription_operation(&self, operation: &str, asset_ids: Vec<String>) -> Value {
        match operation {
            "subscribe" => json!({
                "assets_ids": asset_ids,
                "operation": operation,
                "custom_feature_enabled": true,
            }),
            _ => json!({
                "assets_ids": asset_ids,
                "operation": operation,
            }),
        }
    }

    async fn send_subscription(&mut self) -> Result<()> {
        if self.subscribed_asset_ids.is_empty() {
            return Ok(());
        }

        let message = self.format_subscription();
        self.send_raw_message(message).await
    }

    async fn send_raw_message(&mut self, message: Value) -> Result<()> {
        if let Some(connection) = self.connection.as_mut() {
            let text = serde_json::to_string(&message).map_err(|e| {
                PolyError::parse(
                    format!("Failed to serialize subscription message: {}", e),
                    None,
                )
            })?;
            connection
                .send(Message::Text(text.into()))
                .await
                .map_err(|e| {
                    PolyError::stream(
                        format!("Failed to send message: {}", e),
                        crate::errors::StreamErrorKind::MessageCorrupted,
                    )
                })?;
            return Ok(());
        }
        Err(PolyError::stream(
            "WebSocket connection not established",
            crate::errors::StreamErrorKind::ConnectionFailed,
        ))
    }

    async fn connect(&mut self) -> Result<()> {
        let mut attempts = 0;
        loop {
            match connect_async(&self.connect_url).await {
                Ok((socket, _)) => {
                    self.connection = Some(socket);
                    if attempts > 0 {
                        self.stats.reconnect_count += 1;
                    }
                    return Ok(());
                }
                Err(err) => {
                    attempts += 1;
                    let delay = self.reconnect_delay(attempts);
                    self.stats.errors += 1;
                    if attempts >= MAX_RECONNECT_ATTEMPTS {
                        return Err(PolyError::stream(
                            format!("Failed to connect after {} attempts: {}", attempts, err),
                            crate::errors::StreamErrorKind::ConnectionFailed,
                        ));
                    }
                    sleep(delay).await;
                }
            }
        }
    }

    fn reconnect_delay(&self, attempts: u32) -> Duration {
        let millis = BASE_RECONNECT_DELAY.as_millis() * attempts as u128;

        Duration::from_millis(millis.min(MAX_RECONNECT_DELAY.as_millis()) as u64)
    }

    async fn ensure_connection(&mut self) -> Result<bool> {
        if self.connection.is_none() {
            self.connect().await?;
            self.send_subscription().await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Subscribe to the market channel for the provided token/market IDs.
    pub async fn subscribe(&mut self, asset_ids: Vec<String>) -> Result<()> {
        let next_asset_ids = dedupe_preserve_order(asset_ids);
        let previous_asset_ids = self.subscribed_asset_ids.clone();
        self.subscribed_asset_ids = next_asset_ids.clone();

        let connected = self.ensure_connection().await?;
        if connected {
            return Ok(());
        }

        if previous_asset_ids.is_empty() {
            return self.send_subscription().await;
        }

        let removed_asset_ids = diff_ids(&previous_asset_ids, &next_asset_ids);
        if !removed_asset_ids.is_empty() {
            let message = self.format_subscription_operation("unsubscribe", removed_asset_ids);
            self.send_raw_message(message).await?;
        }

        let added_asset_ids = diff_ids(&next_asset_ids, &previous_asset_ids);
        if !added_asset_ids.is_empty() {
            let message = self.format_subscription_operation("subscribe", added_asset_ids);
            self.send_raw_message(message).await?;
        }

        Ok(())
    }

    /// Read the next market channel event, reconnecting transparently when
    /// the socket drops.
    pub async fn next_event(&mut self) -> Result<WssMarketEvent> {
        let mut ping_interval = interval(KEEPALIVE_INTERVAL);
        ping_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            if let Some(evt) = self.pending_events.pop_front() {
                return Ok(evt);
            }
            self.ensure_connection().await?;

            tokio::select! {
                biased;
                _ = ping_interval.tick() => {
                    if let Some(connection) = self.connection.as_mut() {
                        let _ = connection.send(Message::Text("PING".into())).await;
                    }
                }
                maybe_msg = self.connection.as_mut().unwrap().next() => {
                    match maybe_msg {
                        Some(Ok(Message::Text(text))) => {
                            let trimmed = text.trim();
                            if trimmed.eq_ignore_ascii_case("ping") || trimmed.eq_ignore_ascii_case("pong") {
                                continue;
                            }
                            let first_char = trimmed.chars().next();
                            if first_char != Some('{') && first_char != Some('[') {
                                warn!("ignoring unexpected text frame: {}", trimmed);
                                continue;
                            }
                            let events = parse_market_events(&text)?;
                            self.stats.messages_received += events.len() as u64;
                            self.stats.last_message_time = Some(Utc::now());
                            for evt in events {
                                self.pending_events.push_back(evt);
                            }
                            if let Some(evt) = self.pending_events.pop_front() {
                                return Ok(evt);
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if let Some(connection) = self.connection.as_mut() {
                                let _ = connection.send(Message::Pong(payload)).await;
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) => {
                            self.disconnect_history.push_back(Utc::now());
                            if self.disconnect_history.len() > 5 {
                                self.disconnect_history.pop_front();
                            }
                            self.connection = None;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            warn!("WebSocket error: {}", err);
                            self.connection = None;
                            self.stats.errors += 1;
                        }
                        None => {
                            self.connection = None;
                        }
                    }
                }
            }
        }
    }
}

/// Reconnecting client for the authenticated user channel.
pub struct WssUserClient {
    connect_url: String,
    connection: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    subscribed_markets: Vec<String>,
    stats: WssStats,
    disconnect_history: VecDeque<DateTime<Utc>>,
    pending_events: VecDeque<WssUserEvent>,
    auth: ApiCredentials,
}

impl WssUserClient {
    /// Create a new instance using the default Polymarket WSS base.
    pub fn new(auth: ApiCredentials) -> Self {
        Self::with_url(DEFAULT_WSS_BASE, auth)
    }

    /// Create a new client against a custom endpoint (useful for tests).
    pub fn with_url(url: &str, auth: ApiCredentials) -> Self {
        let trimmed = url.trim_end_matches('/');
        let connect_url = format!("{}{}", trimmed, USER_CHANNEL_PATH);
        Self {
            connection: None,
            subscribed_markets: Vec::new(),
            stats: WssStats::default(),
            disconnect_history: VecDeque::with_capacity(5),
            connect_url,
            pending_events: VecDeque::new(),
            auth,
        }
    }

    /// Access connection stats for observability.
    pub fn stats(&self) -> WssStats {
        self.stats.clone()
    }

    fn format_subscription(&self) -> Option<Value> {
        if self.subscribed_markets.is_empty() {
            return None;
        }

        Some(json!({
            "type": "user",
            "auth": {
                "apiKey": self.auth.api_key,
                "secret": self.auth.secret,
                "passphrase": self.auth.passphrase,
            },
            "markets": self.subscribed_markets,
        }))
    }

    fn format_subscription_operation(&self, operation: &str, markets: Vec<String>) -> Option<Value> {
        if markets.is_empty() {
            return None;
        }

        Some(json!({
            "markets": markets,
            "operation": operation,
        }))
    }

    async fn send_subscription(&mut self) -> Result<()> {
        if let Some(message) = self.format_subscription() {
            self.send_raw_message(message).await
        } else {
            Ok(())
        }
    }

    async fn send_raw_message(&mut self, message: Value) -> Result<()> {
        if let Some(connection) = self.connection.as_mut() {
            let text = serde_json::to_string(&message).map_err(|e| {
                PolyError::parse(
                    format!("Failed to serialize subscription message: {}", e),
                    None,
                )
            })?;
            connection
                .send(Message::Text(text.into()))
                .await
                .map_err(|e| {
                    PolyError::stream(
                        format!("Failed to send message: {}", e),
                        crate::errors::StreamErrorKind::MessageCorrupted,
                    )
                })?;
            return Ok(());
        }
        Err(PolyError::stream(
            "WebSocket connection not established",
            crate::errors::StreamErrorKind::ConnectionFailed,
        ))
    }

    async fn connect(&mut self) -> Result<()> {
        let mut attempts = 0;
        loop {
            match connect_async(&self.connect_url).await {
                Ok((socket, _)) => {
                    self.connection = Some(socket);
                    if attempts > 0 {
                        self.stats.reconnect_count += 1;
                    }
                    return Ok(());
                }
                Err(err) => {
                    attempts += 1;
                    let delay = self.reconnect_delay(attempts);
                    self.stats.errors += 1;
                    if attempts >= MAX_RECONNECT_ATTEMPTS {
                        return Err(PolyError::stream(
                            format!("Failed to connect after {} attempts: {}", attempts, err),
                            crate::errors::StreamErrorKind::ConnectionFailed,
                        ));
                    }
                    sleep(delay).await;
                }
            }
        }
    }

    fn reconnect_delay(&self, attempts: u32) -> Duration {
        let millis = BASE_RECONNECT_DELAY.as_millis() * attempts as u128;

        Duration::from_millis(millis.min(MAX_RECONNECT_DELAY.as_millis()) as u64)
    }

    async fn ensure_connection(&mut self) -> Result<bool> {
        if self.connection.is_none() {
            self.connect().await?;
            self.send_subscription().await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Subscribe to the user channel for the provided market IDs.
    pub async fn subscribe(&mut self, market_ids: Vec<String>) -> Result<()> {
        let next_market_ids = dedupe_preserve_order(market_ids);
        let previous_market_ids = self.subscribed_markets.clone();
        self.subscribed_markets = next_market_ids.clone();

        let connected = self.ensure_connection().await?;
        if connected {
            return Ok(());
        }

        if previous_market_ids.is_empty() {
            return self.send_subscription().await;
        }

        let removed_markets = diff_ids(&previous_market_ids, &next_market_ids);
        if let Some(message) = self.format_subscription_operation("unsubscribe", removed_markets) {
            self.send_raw_message(message).await?;
        }

        let added_markets = diff_ids(&next_market_ids, &previous_market_ids);
        if let Some(message) = self.format_subscription_operation("subscribe", added_markets) {
            self.send_raw_message(message).await?;
        }

        Ok(())
    }

    /// Read the next user channel event, reconnecting transparently when the
    /// socket drops.
    pub async fn next_event(&mut self) -> Result<WssUserEvent> {
        let mut ping_interval = interval(KEEPALIVE_INTERVAL);
        ping_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            if let Some(evt) = self.pending_events.pop_front() {
                return Ok(evt);
            }
            self.ensure_connection().await?;

            tokio::select! {
                biased;
                _ = ping_interval.tick() => {
                    if let Some(connection) = self.connection.as_mut() {
                        let _ = connection.send(Message::Text("PING".into())).await;
                    }
                }
                maybe_msg = self.connection.as_mut().unwrap().next() => {
                    match maybe_msg {
                        Some(Ok(Message::Text(text))) => {
                            let trimmed = text.trim();
                            if trimmed.eq_ignore_ascii_case("ping") || trimmed.eq_ignore_ascii_case("pong") {
                                continue;
                            }
                            let first_char = trimmed.chars().next();
                            if first_char != Some('{') && first_char != Some('[') {
                                warn!("ignoring unexpected text frame: {}", trimmed);
                                continue;
                            }
                            let events = parse_user_events(&text)?;
                            self.stats.messages_received += events.len() as u64;
                            self.stats.last_message_time = Some(Utc::now());
                            for evt in events {
                                self.pending_events.push_back(evt);
                            }
                            if let Some(evt) = self.pending_events.pop_front() {
                                return Ok(evt);
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if let Some(connection) = self.connection.as_mut() {
                                let _ = connection.send(Message::Pong(payload)).await;
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) => {
                            self.disconnect_history.push_back(Utc::now());
                            if self.disconnect_history.len() > 5 {
                                self.disconnect_history.pop_front();
                            }
                            self.connection = None;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            warn!("WebSocket error: {}", err);
                            self.connection = None;
                            self.stats.errors += 1;
                        }
                        None => {
                            self.connection = None;
                        }
                    }
                }
            }
        }
    }
}

fn parse_market_events(text: &str) -> Result<Vec<WssMarketEvent>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|err| PolyError::parse(format!("Invalid JSON: {}", err), Some(Box::new(err))))?;

    if let Some(array) = value.as_array() {
        array
            .iter()
            .map(parse_market_event_value)
            .collect::<Result<Vec<_>>>()
    } else {
        Ok(vec![parse_market_event_value(&value)?])
    }
}

fn parse_market_event_value(value: &Value) -> Result<WssMarketEvent> {
    let event_type = value
        .get("event_type")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("type").and_then(|v| v.as_str()))
        .ok_or_else(|| PolyError::parse("Missing event_type/type in market message", None))?;

    match event_type {
        "book" => {
            let parsed: MarketBook = serde_json::from_value(value.clone()).map_err(|err| {
                PolyError::parse(
                    format!("Failed to parse book message: {}", err),
                    Some(Box::new(err)),
                )
            })?;
            Ok(WssMarketEvent::Book(parsed))
        }
        "price_change" => {
            let parsed =
                serde_json::from_value::<PriceChangeMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse price_change: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssMarketEvent::PriceChange(parsed))
        }
        "tick_size_change" => {
            let parsed =
                serde_json::from_value::<TickSizeChangeMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse tick_size_change: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssMarketEvent::TickSizeChange(parsed))
        }
        "last_trade_price" => {
            let parsed =
                serde_json::from_value::<LastTradeMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse last_trade_price: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssMarketEvent::LastTrade(parsed))
        }
        "best_bid_ask" => {
            let parsed =
                serde_json::from_value::<BestBidAskMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse best_bid_ask: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssMarketEvent::BestBidAsk(parsed))
        }
        "new_market" => {
            let parsed =
                serde_json::from_value::<NewMarketMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse new_market: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssMarketEvent::NewMarket(parsed))
        }
        "market_resolved" => {
            let parsed =
                serde_json::from_value::<MarketResolvedMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse market_resolved: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssMarketEvent::MarketResolved(parsed))
        }
        other => Err(PolyError::parse(
            format!("Unknown market event_type: {}", other),
            None,
        )),
    }
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn diff_ids(next: &[String], previous: &[String]) -> Vec<String> {
    let previous_set = previous.iter().cloned().collect::<HashSet<_>>();
    next.iter()
        .filter(|value| !previous_set.contains(*value))
        .cloned()
        .collect()
}

fn parse_user_events(text: &str) -> Result<Vec<WssUserEvent>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|err| PolyError::parse(format!("Invalid JSON: {}", err), Some(Box::new(err))))?;

    if let Some(array) = value.as_array() {
        array
            .iter()
            .map(parse_user_event_value)
            .collect::<Result<Vec<_>>>()
    } else {
        Ok(vec![parse_user_event_value(&value)?])
    }
}

fn parse_user_event_value(value: &Value) -> Result<WssUserEvent> {
    let event_type = value
        .get("event_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PolyError::parse("Missing event_type in user message", None))?;

    match event_type {
        "trade" => {
            let parsed =
                serde_json::from_value::<WssUserTradeMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse user trade message: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssUserEvent::Trade(parsed))
        }
        "order" => {
            let parsed =
                serde_json::from_value::<WssUserOrderMessage>(value.clone()).map_err(|err| {
                    PolyError::parse(
                        format!("Failed to parse user order message: {}", err),
                        Some(Box::new(err)),
                    )
                })?;
            Ok(WssUserEvent::Order(parsed))
        }
        other => Err(PolyError::parse(
            format!("Unknown user event_type: {}", other),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::accept_async;

    async fn spawn_text_capture_server() -> Result<(String, mpsc::UnboundedReceiver<String>)> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|err| PolyError::stream(
                format!("failed to bind test websocket listener: {err}"),
                crate::errors::StreamErrorKind::ConnectionFailed,
            ))?;
        let addr = listener
            .local_addr()
            .map_err(|err| PolyError::stream(
                format!("failed to get local addr: {err}"),
                crate::errors::StreamErrorKind::ConnectionFailed,
            ))?;
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut websocket) = accept_async(stream).await {
                    while let Some(message) = websocket.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                let _ = tx.send(text.to_string());
                            }
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok((format!("ws://{}", addr), rx))
    }

    #[tokio::test]
    async fn market_subscribe_sends_single_initial_payload_with_custom_feature() {
        let (url, mut rx) = spawn_text_capture_server().await.expect("server should start");
        let mut client = WssMarketClient::with_url(&url);

        client
            .subscribe(vec!["asset-a".to_string(), "asset-b".to_string()])
            .await
            .expect("subscribe should succeed");

        let first = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected initial subscription message")
            .expect("initial subscription should be captured");
        let payload: Value = serde_json::from_str(&first).expect("subscription should be JSON");

        assert_eq!(
            payload,
            json!({
                "type": "market",
                "assets_ids": ["asset-a", "asset-b"],
                "custom_feature_enabled": true
            })
        );

        assert!(
            timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
            "fresh subscribe should not send a duplicate initial subscription"
        );
    }

    #[tokio::test]
    async fn market_resubscribe_sends_dynamic_subscribe_operation_for_added_assets() {
        let (url, mut rx) = spawn_text_capture_server().await.expect("server should start");
        let mut client = WssMarketClient::with_url(&url);

        client
            .subscribe(vec!["asset-a".to_string()])
            .await
            .expect("initial subscribe should succeed");
        let _ = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected initial subscription message");
        assert!(timeout(Duration::from_millis(250), rx.recv()).await.is_err());

        client
            .subscribe(vec!["asset-a".to_string(), "asset-b".to_string()])
            .await
            .expect("resubscribe should succeed");

        let second = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected subscribe operation message")
            .expect("dynamic subscribe message should be captured");
        let payload: Value = serde_json::from_str(&second).expect("dynamic message should be JSON");

        assert_eq!(
            payload,
            json!({
                "assets_ids": ["asset-b"],
                "operation": "subscribe",
                "custom_feature_enabled": true
            })
        );

        assert!(
            timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
            "resubscribe should emit only one delta message for added assets"
        );
    }

    #[tokio::test]
    async fn market_resubscribe_sends_dynamic_unsubscribe_operation_for_removed_assets() {
        let (url, mut rx) = spawn_text_capture_server().await.expect("server should start");
        let mut client = WssMarketClient::with_url(&url);

        client
            .subscribe(vec!["asset-a".to_string(), "asset-b".to_string()])
            .await
            .expect("initial subscribe should succeed");
        let _ = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected initial subscription message");
        assert!(timeout(Duration::from_millis(250), rx.recv()).await.is_err());

        client
            .subscribe(vec!["asset-b".to_string()])
            .await
            .expect("resubscribe should succeed");

        let second = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected unsubscribe operation message")
            .expect("dynamic unsubscribe message should be captured");
        let payload: Value = serde_json::from_str(&second).expect("dynamic message should be JSON");

        assert_eq!(
            payload,
            json!({
                "assets_ids": ["asset-a"],
                "operation": "unsubscribe"
            })
        );

        assert!(
            timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
            "unsubscribe should emit only one delta message for removed assets"
        );
    }

    #[tokio::test]
    async fn user_subscribe_sends_single_initial_payload_without_duplicate() {
        let (url, mut rx) = spawn_text_capture_server().await.expect("server should start");
        let auth = ApiCredentials {
            api_key: "api-key".to_string(),
            secret: "secret".to_string(),
            passphrase: "passphrase".to_string(),
        };
        let mut client = WssUserClient::with_url(&url, auth.clone());

        client
            .subscribe(vec!["market-a".to_string()])
            .await
            .expect("user subscribe should succeed");

        let first = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected initial user subscription message")
            .expect("initial user subscription should be captured");
        let payload: Value = serde_json::from_str(&first).expect("subscription should be JSON");

        assert_eq!(
            payload,
            json!({
                "type": "user",
                "auth": {
                    "apiKey": auth.api_key,
                    "secret": auth.secret,
                    "passphrase": auth.passphrase,
                },
                "markets": ["market-a"]
            })
        );

        assert!(
            timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
            "fresh user subscribe should not send a duplicate initial subscription"
        );
    }

    #[tokio::test]
    async fn user_resubscribe_sends_dynamic_market_subscribe_operation() {
        let (url, mut rx) = spawn_text_capture_server().await.expect("server should start");
        let auth = ApiCredentials {
            api_key: "api-key".to_string(),
            secret: "secret".to_string(),
            passphrase: "passphrase".to_string(),
        };
        let mut client = WssUserClient::with_url(&url, auth);

        client
            .subscribe(vec!["market-a".to_string()])
            .await
            .expect("initial user subscribe should succeed");
        let _ = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected initial user subscription");
        assert!(timeout(Duration::from_millis(250), rx.recv()).await.is_err());

        client
            .subscribe(vec!["market-a".to_string(), "market-b".to_string()])
            .await
            .expect("user resubscribe should succeed");

        let second = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected dynamic user subscribe operation")
            .expect("dynamic user subscribe should be captured");
        let payload: Value = serde_json::from_str(&second).expect("dynamic user message should be JSON");

        assert_eq!(
            payload,
            json!({
                "markets": ["market-b"],
                "operation": "subscribe"
            })
        );

        assert!(
            timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
            "user resubscribe should emit only one delta message"
        );
    }

    #[test]
    fn parse_market_events_supports_best_bid_ask_and_market_lifecycle_messages() {
        let text = json!([
            {
                "event_type": "best_bid_ask",
                "market": "market-1",
                "asset_id": "asset-1",
                "best_bid": "0.73",
                "best_ask": "0.77",
                "spread": "0.04",
                "timestamp": "1766789469958"
            },
            {
                "event_type": "new_market",
                "id": "1031769",
                "question": "Will NVIDIA (NVDA) close above $240 end of January?",
                "market": "0x311d0c4b",
                "slug": "nvda-above-240-on-january-30-2026",
                "description": "This market will resolve to Yes if the official closing price...",
                "assets_ids": ["asset-yes", "asset-no"],
                "outcomes": ["Yes", "No"],
                "event_message": {
                    "id": "125819",
                    "ticker": "nvda-above-in-january-2026",
                    "slug": "nvda-above-in-january-2026",
                    "title": "Will NVIDIA (NVDA) close above ___ end of January?"
                },
                "timestamp": "1766790415550"
            },
            {
                "event_type": "market_resolved",
                "id": "1031769",
                "question": "Will NVIDIA (NVDA) close above $240 end of January?",
                "market": "0x311d0c4b",
                "slug": "nvda-above-240-on-january-30-2026",
                "description": "This market will resolve to Yes if the official closing price...",
                "assets_ids": ["asset-yes", "asset-no"],
                "outcomes": ["Yes", "No"],
                "winning_asset_id": "asset-yes",
                "winning_outcome": "Yes",
                "event_message": {
                    "id": "125819",
                    "ticker": "nvda-above-in-january-2026"
                },
                "timestamp": "1766790415550"
            }
        ])
        .to_string();

        let events = parse_market_events(&text).expect("official market channel events should parse");

        let rendered = events
            .iter()
            .map(|event| format!("{event:?}"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert!(rendered[0].contains("BestBidAsk"));
        assert!(rendered[1].contains("NewMarket"));
        assert!(rendered[2].contains("MarketResolved"));
    }
}
