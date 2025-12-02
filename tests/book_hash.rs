use polysqueeze::client::ClobClient;
use polysqueeze::errors::{PolyError, Result};
use polysqueeze::wss::{MarketBook, PriceChangeMessage, WssMarketClient, WssMarketEvent};
use serde_json::{self, json};
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};

const RUN_ENV: &str = "RUN_BOOK_HASH_TEST";
const LOG_FILE_ENV: &str = "POLY_BOOK_HASH_LOG";

fn should_run() -> bool {
    env::var(RUN_ENV).is_ok()
}

/// Integration test that relies on live network access; enable with RUN_BOOK_HASH_TEST=1
/// and set POLY_BOOK_HASH_TOKEN_ID to the asset/token you want to verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn market_book_hash_matches_rest() -> Result<()> {
    if !should_run() {
        eprintln!("Skipping book hash test (set {RUN_ENV}=1)");
        return Ok(());
    }

    let token_id = env::var("POLY_BOOK_HASH_TOKEN_ID").unwrap_or_default();
    if token_id.is_empty() {
        eprintln!("POLY_BOOK_HASH_TOKEN_ID is empty; provide a token id to run this test.");
        return Ok(());
    }

    let mut logger = BookLogger::new()?;
    let base_url =
        env::var("POLY_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
    let mut market_client = WssMarketClient::new();
    let subscribe_started = Instant::now();
    market_client.subscribe(vec![token_id.clone()]).await?;

    let initial_book = timeout(
        Duration::from_secs(30),
        wait_for_initial_book(&mut market_client, &token_id, &mut logger),
    )
    .await
    .map_err(|_| PolyError::timeout(Duration::from_secs(30), "initial market book"))??;

    let mut latest_ts = parse_timestamp(&initial_book)?;
    let mut latest_hash = initial_book.hash.clone();
    let mut last_event_hash = latest_hash.clone();

    let elapsed = subscribe_started.elapsed();
    if elapsed < Duration::from_secs(1) {
        sleep(Duration::from_secs(1) - elapsed).await;
    }

    let rest_book = ClobClient::new(&base_url).get_order_book(&token_id).await?;

    if rest_book.timestamp <= latest_ts {
        println!(
            "/book timestamp {} older or equal to WSS timestamp {}; exiting test.",
            rest_book.timestamp, latest_ts
        );
        return Ok(());
    }

    println!(
        "/book timestamp {} is newer than WSS timestamp {}; waiting for hash sync",
        rest_book.timestamp, latest_ts
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if latest_hash == rest_book.hash {
            println!("Hashes already matched at timestamp {}", latest_ts);
            return Ok(());
        }

        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| Duration::from_millis(0));
        if remaining.is_zero() {
            break;
        }

        let event = timeout(
            remaining,
            wait_for_tracked_event(&mut market_client, &token_id, &mut logger),
        )
        .await
        .map_err(|_| PolyError::timeout(remaining, "waiting for matching market event"))??;

        match event {
            TrackedEvent::Book(book) => {
                latest_ts = parse_timestamp(&book)?;
                latest_hash = book.hash.clone();
                last_event_hash = latest_hash.clone();
                if latest_hash == rest_book.hash {
                    println!(
                        "Hash {} matched via book (WSS timestamp {}, REST timestamp {})",
                        latest_hash, latest_ts, rest_book.timestamp
                    );
                    return Ok(());
                }
            }
            TrackedEvent::PriceChange { timestamp, hashes } => {
                for hash in hashes {
                    if hash == rest_book.hash {
                        println!(
                            "Hash {} matched via price_change (WSS timestamp {}, REST timestamp {})",
                            hash, timestamp, rest_book.timestamp
                        );
                        return Ok(());
                    }
                    last_event_hash = hash;
                }
            }
        }
    }

    println!(
        "Hash mismatch after 5s: REST hash {} vs latest market-event hash {}",
        rest_book.hash, last_event_hash
    );
    panic!("REST/WSS hashes failed to match within 5 seconds");
}

async fn wait_for_initial_book(
    client: &mut WssMarketClient,
    token_id: &str,
    logger: &mut BookLogger,
) -> Result<MarketBook> {
    loop {
        match client.next_event().await? {
            WssMarketEvent::Book(book) => {
                if token_id.is_empty() || book.asset_id == token_id {
                    logger.log_book(&book)?;
                    return Ok(book);
                }
            }
            _ => continue,
        }
    }
}

enum TrackedEvent {
    Book(MarketBook),
    PriceChange { timestamp: u64, hashes: Vec<String> },
}

async fn wait_for_tracked_event(
    client: &mut WssMarketClient,
    token_id: &str,
    logger: &mut BookLogger,
) -> Result<TrackedEvent> {
    loop {
        match client.next_event().await? {
            WssMarketEvent::Book(book) => {
                if token_id.is_empty() || book.asset_id == token_id {
                    logger.log_book(&book)?;
                    return Ok(TrackedEvent::Book(book));
                }
            }
            WssMarketEvent::PriceChange(change) => {
                let hashes: Vec<String> = change
                    .price_changes
                    .iter()
                    .filter(|entry| token_id.is_empty() || entry.asset_id == token_id)
                    .map(|entry| entry.hash.clone())
                    .collect();
                if hashes.is_empty() {
                    continue;
                }
                logger.log_price_change(&change)?;
                let timestamp = parse_price_change_timestamp(&change)?;
                return Ok(TrackedEvent::PriceChange { timestamp, hashes });
            }
            _ => continue,
        }
    }
}

fn parse_timestamp(book: &MarketBook) -> Result<u64> {
    book.timestamp.parse::<u64>().map_err(|e| {
        PolyError::parse(
            format!(
                "Invalid timestamp '{}' in market book update",
                book.timestamp
            ),
            Some(Box::new(e)),
        )
    })
}

fn parse_price_change_timestamp(change: &PriceChangeMessage) -> Result<u64> {
    change.timestamp.parse::<u64>().map_err(|e| {
        PolyError::parse(
            format!(
                "Invalid timestamp '{}' in price change update",
                change.timestamp
            ),
            Some(Box::new(e)),
        )
    })
}

struct BookLogger {
    file: std::fs::File,
    path: String,
}

impl BookLogger {
    fn new() -> Result<Self> {
        let path =
            env::var(LOG_FILE_ENV).unwrap_or_else(|_| "book_hash_market_events.log".to_string());
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| PolyError::internal(format!("Failed to open log file {}", path), e))?;
        println!("Logging market events to {}", path);
        Ok(Self { file, path })
    }

    fn log_book(&mut self, book: &MarketBook) -> Result<()> {
        self.log_entry("book", book)
    }

    fn log_price_change(&mut self, change: &PriceChangeMessage) -> Result<()> {
        self.log_entry("price_change", change)
    }

    fn log_entry<T: serde::Serialize>(&mut self, event: &str, payload: &T) -> Result<()> {
        let entry = json!({
            "event": event,
            "payload": payload
        });
        let line = serde_json::to_string(&entry).map_err(|e| {
            PolyError::parse(
                format!("Failed to serialize {} event for logging", event),
                Some(Box::new(e)),
            )
        })?;
        writeln!(self.file, "{line}").map_err(|e| {
            PolyError::internal(format!("Failed to write {} log {}", event, self.path), e)
        })?;
        self.file.flush().map_err(|e| {
            PolyError::internal(format!("Failed to flush {} log {}", event, self.path), e)
        })
    }
}
