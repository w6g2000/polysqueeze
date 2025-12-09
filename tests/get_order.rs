use polysqueeze::client::ClobClient;
use polysqueeze::errors::Result;
use polysqueeze::types::ApiCredentials;
use std::env;

fn env_var(key: &str) -> String {
    env::var(key).expect(&format!("{} must be set for get_order test", key))
}

fn should_run() -> bool {
    env::var("RUN_GET_ORDER_TEST").is_ok()
}

#[tokio::test]
async fn get_order_live_smoke() -> Result<()> {
    if !should_run() {
        eprintln!("Skipping get_order_live_smoke (set RUN_GET_ORDER_TEST=1)");
        return Ok(());
    }

    let base_url =
        env::var("POLY_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".into());
    let private_key = env_var("POLY_PRIVATE_KEY");
    let api_key = env_var("POLY_API_KEY");
    let api_secret = env_var("POLY_API_SECRET");
    let api_passphrase = env_var("POLY_API_PASSPHRASE");
    let order_id = env_var("POLY_ORDER_ID");
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

    let order = client.get_order(&order_id).await?;
    assert_eq!(order.id, order_id);

    Ok(())
}
