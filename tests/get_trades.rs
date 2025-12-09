use polysqueeze::client::ClobClient;
use polysqueeze::errors::{PolyError, Result};
use polysqueeze::types::ApiCredentials;
use std::env;
use std::fs;

fn env_var(key: &str) -> String {
    env::var(key).expect(&format!("{} must be set for get_trades test", key))
}

fn should_run() -> bool {
    env::var("RUN_GET_TRADES_TEST").is_ok()
}

#[tokio::test]
async fn get_trades_live_smoke() -> Result<()> {
    if !should_run() {
        eprintln!("Skipping get_trades_live_smoke (set RUN_GET_TRADES_TEST=1)");
        return Ok(());
    }

    let base_url =
        env::var("POLY_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".into());
    let private_key = env_var("POLY_PRIVATE_KEY");
    let api_key = env_var("POLY_API_KEY");
    let api_secret = env_var("POLY_API_SECRET");
    let api_passphrase = env_var("POLY_API_PASSPHRASE");
    let chain_id = env::var("POLY_CHAIN_ID")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(137);

    let api_creds = ApiCredentials {
        api_key,
        secret: api_secret,
        passphrase: api_passphrase,
    };
    let client = ClobClient::with_l2_headers(&base_url, &private_key, chain_id, api_creds);

    // No strict assertions on contents; call should succeed and return a list (possibly empty).
    let trades = client.get_trades(None, None).await?;
    println!("Fetched {} trades", trades.len());

    let serialized = serde_json::to_string_pretty(&trades)
        .map_err(|e| PolyError::parse(format!("Failed to serialize trades: {}", e), None))?;
    fs::write("trades_response.json", serialized).map_err(|e| {
        PolyError::parse(format!("Failed to write trades_response.json: {}", e), None)
    })?;

    Ok(())
}
