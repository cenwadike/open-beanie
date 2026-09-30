// src/evm_keeper.rs

use anyhow::{Context, Result as AnyhowResult};
use ethers::{
    contract::abigen,
    middleware::{NonceManagerMiddleware, SignerMiddleware},
    providers::{Http, Middleware, Provider, RetryClient},
    signers::{LocalWallet, Signer},
    types::{Address, H256},
};
use std::sync::Arc;

use crate::config::EvmConfig;

abigen!(
    ChainXReceiver,
    r#"[
        function sweep() external returns (uint256 net, uint256 feeToCaller, uint256 feeToTreasury, uint256 fee)
        function initialized() external view returns (bool)
    ]"#
);

abigen!(
    Erc20,
    r#"[
        function balanceOf(address) external view returns (uint256)
        event Transfer(address indexed from, address indexed to, uint256 value)
    ]"#
);

abigen!(
    Erc20Domain,
    r#"[function DOMAIN_SEPARATOR() external view returns (bytes32)]"#
);

pub const MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

abigen!(
    Multicall3,
    r#"[
      {
        "inputs": [
          {
            "components": [
              { "internalType": "address", "name": "target", "type": "address" },
              { "internalType": "bool", "name": "allowFailure", "type": "bool" },
              { "internalType": "bytes", "name": "callData", "type": "bytes" }
            ],
            "internalType": "struct Multicall3.Call3[]",
            "name": "calls",
            "type": "tuple[]"
          }
        ],
        "name": "aggregate3",
        "outputs": [
          {
            "components": [
              { "internalType": "bool", "name": "success", "type": "bool" },
              { "internalType": "bytes", "name": "returnData", "type": "bytes" }
            ],
            "internalType": "struct Multicall3.Result[]",
            "name": "returnData",
            "type": "tuple[]"
          }
        ],
        "stateMutability": "payable",
        "type": "function"
      }
    ]"#
);

pub type SignerProvider =
    SignerMiddleware<NonceManagerMiddleware<Provider<RetryClient<Http>>>, LocalWallet>;

pub async fn build_client(cfg: &EvmConfig) -> AnyhowResult<Arc<SignerProvider>> {
    let provider = Provider::<RetryClient<Http>>::new_client(cfg.evm_rpc_url.as_str(), 10, 2000)
        .context("invalid RPC URL or failed initializing retry client")?;

    let chain_id = provider
        .get_chainid()
        .await
        .context("failed fetching chain id")?
        .as_u64();

    let wallet = cfg.keeper_wallet.clone().with_chain_id(chain_id);
    let address = wallet.address();

    let nonce_managed = NonceManagerMiddleware::new(provider, address);
    Ok(Arc::new(SignerMiddleware::new(nonce_managed, wallet)))
}

pub async fn fetch_domain_separator(
    client: Arc<SignerProvider>,
    token_address: Address,
) -> AnyhowResult<H256> {
    let contract = Erc20Domain::new(token_address, client);
    let domain_separator_bytes = contract.domain_separator().call().await?;

    Ok(H256::from(domain_separator_bytes))
}
