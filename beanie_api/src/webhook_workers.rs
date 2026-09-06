use std::sync::Arc;
use tokio::sync::mpsc;

use reqwest::Client;

use crate::models::WebhookJob;

pub async fn run_webhook_worker(http_client: Arc<Client>, mut rx: mpsc::Receiver<WebhookJob>) {
    println!("webhook worker starting...");

    while let Some(job) = rx.recv().await {
        let cfg = job.cfg.clone();
        let url = job.webhook_url.clone();
        let deposit = job.deposit.clone();
        let sweep_tx = job.sweep_tx.as_deref();
        let retries = job.max_retries;

        if let Err(e) = beanie_keeper::webhook::deliver_deposit(
            &http_client,
            &cfg,
            &url,
            &deposit,
            sweep_tx,
            retries,
        )
        .await
        {
            eprintln!(
                "webhook job failed for {} -> {}: {:#}",
                deposit.tx_hash, url, e
            );
        }
    }
}
