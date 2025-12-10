use polysqueeze::{client::ClobClient, errors::Result, types::ApiCredentials};
use std::env;

fn env_or_skip(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!("Skipping get_active_orders_live (set {key})");
            None
        }
    }
}

#[tokio::test]
async fn get_active_orders_live() -> Result<()> {
    if env::var("RUN_ACTIVE_ORDERS_TEST").is_err() {
        eprintln!("Skipping get_active_orders_live (set RUN_ACTIVE_ORDERS_TEST=1)");
        return Ok(());
    }

    let base_url =
        env::var("POLY_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".into());
    let private_key = match env_or_skip("POLY_PRIVATE_KEY") {
        Some(v) => v,
        None => return Ok(()),
    };
    let api_key = match env_or_skip("POLY_API_KEY") {
        Some(v) => v,
        None => return Ok(()),
    };
    let api_secret = match env_or_skip("POLY_API_SECRET") {
        Some(v) => v,
        None => return Ok(()),
    };
    let api_passphrase = match env_or_skip("POLY_API_PASSPHRASE") {
        Some(v) => v,
        None => return Ok(()),
    };
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

    // No filters: fetch active orders for the authenticated user
    let orders = client.get_active_orders(None).await?;
    println!("active orders fetched: {}", orders.len());
    println!("{:#?}", orders);

    Ok(())
}
