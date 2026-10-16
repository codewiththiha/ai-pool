//! Seeds keys, lists them censored, and decrypts one on demand.

use ai_pool::{AiClient, ExposeSecret, RotationMode};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // RAM store and ephemeral master key: nothing touches disk.
    let client = AiClient::builder()
        .default_url("https://generativelanguage.googleapis.com/v1beta/openai/")
        .rotation_mode(RotationMode::Proactive)
        .concurrency_limit(50)
        .request_timeout(Duration::from_secs(60))
        .api_keys(vec!["sk-demo-key-000000001", "sk-demo-key-000000002"])
        .build()
        .await?;

    // `list_keys` never returns plaintext; safe to render anywhere.
    for meta in client.manager().list_keys().await? {
        println!("{}", serde_json::to_string_pretty(&meta)?);
    }

    // When plaintext is genuinely needed, ask for it explicitly.
    let metas = client.manager().list_keys().await?;
    let secret = client.manager().get_decrypted_key(&metas[0].id).await?;
    println!("decrypted: {}", secret.expose_secret());

    Ok(())
}
