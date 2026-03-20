use polysqueeze::Result;
use polysqueeze::client::ClobClient;
use polysqueeze::errors::PolyError;
use polysqueeze::types::{GammaListParams, Market};
use polysqueeze::wss::{WssMarketClient, WssMarketEvent};
use rust_decimal::Decimal;
use std::env;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::main]
async fn main() -> Result<()> {
    let base_url =
        env::var("POLY_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
    let clob = ClobClient::new(&base_url);

    let min_liquidity = env::var("POLY_WSS_MIN_LIQUIDITY")
        .ok()
        .and_then(|value| Decimal::from_str(&value).ok())
        .unwrap_or_else(|| Decimal::from(1_000_000));

    let params = GammaListParams {
        limit: Some(50),
        liquidity_num_min: Some(min_liquidity),
        ..Default::default()
    };

    let response = clob.get_markets(None, Some(&params)).await?;
    let market = pick_liquid_market(&response.data, min_liquidity)?;

    println!(
        "Selected market {} (liquidity={:?})",
        market.condition_id, market.liquidity_num
    );

    let asset_ids = derive_asset_ids(market).unwrap_or_else(|| Vec::new());

    if asset_ids.is_empty() {
        return Err(PolyError::validation(
            "failed to derive asset IDs for the selected market",
        ));
    }

    let mut client = WssMarketClient::new();
    client.subscribe(asset_ids.clone()).await?;

    println!("Subscribed to market channel for assets={:?}", asset_ids);

    for _ in 0..20 {
        match client.next_event().await {
            Ok(WssMarketEvent::PriceChange(change)) => {
                println!(
                    "price_change for {}: {:?}",
                    change.market, change.price_changes
                );
            }
            Ok(WssMarketEvent::Book(book)) => {
                println!(
                    "book {} bids={} asks={}",
                    book.market,
                    book.bids.len(),
                    book.asks.len()
                );
            }
            Ok(WssMarketEvent::TickSizeChange(change)) => {
                println!(
                    "tick size change {} from {} to {}",
                    change.market, change.old_tick_size, change.new_tick_size
                );
            }
            Ok(WssMarketEvent::LastTrade(trade)) => {
                println!(
                    "last_trade {} {:?}@{}",
                    trade.market, trade.side, trade.price
                );
            }
            Ok(WssMarketEvent::BestBidAsk(update)) => {
                println!(
                    "best_bid_ask {} bid={} ask={} spread={}",
                    update.market, update.best_bid, update.best_ask, update.spread
                );
            }
            Ok(WssMarketEvent::NewMarket(market)) => {
                println!(
                    "new_market {} slug={} assets={:?}",
                    market.market, market.slug, market.assets_ids
                );
            }
            Ok(WssMarketEvent::MarketResolved(market)) => {
                println!(
                    "market_resolved {} winner={} ({})",
                    market.market, market.winning_asset_id, market.winning_outcome
                );
            }
            Err(err) => {
                eprintln!("stream error: {}", err);
                break;
            }
        }
    }

    Ok(())
}

fn pick_liquid_market(markets: &[Market], min_liquidity: Decimal) -> Result<&Market> {
    let eligible: Vec<&Market> = markets
        .iter()
        .filter(|m| m.liquidity_num.unwrap_or_default() >= min_liquidity)
        .collect();

    if eligible.is_empty() {
        return Err(PolyError::validation("no liquid markets available"));
    }

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize)
        .unwrap_or(0);
    let idx = seed % eligible.len();
    Ok(eligible[idx])
}

fn derive_asset_ids(market: &Market) -> Option<Vec<String>> {
    if !market.clob_token_ids.is_empty() {
        return Some(market.clob_token_ids.clone());
    }

    let ids = market
        .tokens
        .iter()
        .map(|token| token.token_id.clone())
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();

    if ids.is_empty() { None } else { Some(ids) }
}
