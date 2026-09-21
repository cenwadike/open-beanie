//! Starknet tip streaming — plain block-number polling.
//! Apibara's gRPC stream is already the discovery engine in
//! starknet_apibara.rs; running a second one here just to notice "a new
//! block exists" is the duplication this module used to contain. One job
//! now: tell the caller a new block number showed up.

use crate::starknet_indexer::StarknetTip;
use crate::starknet_keeper::StarknetAccount;
use starknet::{accounts::ConnectedAccount, providers::Provider};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_secs(4);

pub async fn run_starknet_tip_source(
    _ws_url: Option<String>, // kept for call-site compatibility, unused — see module doc
    account: Arc<StarknetAccount>,
    tips_tx: mpsc::Sender<StarknetTip>,
) {
    let mut last_sent = None;
    loop {
        match account.provider().block_number().await {
            Ok(bn) if Some(bn) != last_sent => {
                last_sent = Some(bn);
                if tips_tx
                    .send(StarknetTip { block_number: bn })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("[starknet-tip] block_number poll failed: {e:#}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
