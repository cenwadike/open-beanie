use ethers::types::Address;
use starknet::core::types::Felt;
use std::net::SocketAddr;
use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub rate_limit_per_hour: u32,
    pub listen_addr: SocketAddr,

    // EVM Config
    pub evm_rpc_url: String,
    pub evm_keeper_private_key: String,
    pub evm_factory_address: Address,
    pub webhook_registry_address: Option<Address>,

    // Starknet Config
    pub starknet_rpc_url: Url,
    pub starknet_account_address: Felt,
    pub starknet_private_key: Felt,
    pub starknet_chain_id: Felt,
    pub starknet_factory_address: Felt,

    // Optional Lit Protocol Config
    pub lit_relay_url: String,
    pub lit_cosigner_pubkey: String,
    pub lit_api_key: String,
    pub lit_action_ipfs_cid: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let evm_rpc_url = std::env::var("RPC_URL")?;
        let evm_keeper_private_key = std::env::var("KEEPER_PRIVATE_KEY")?;
        let evm_factory_address = std::env::var("FACTORY_ADDRESS")?.parse()?;
        let webhook_registry_address = std::env::var("WEBHOOK_REGISTRY_ADDRESS")
            .ok()
            .map(|addr| addr.parse())
            .transpose()?;

        let starknet_rpc_url = Url::parse(&std::env::var("STARKNET_RPC_URL")?)?;
        let starknet_account_address = Felt::from_hex(&std::env::var("STARKNET_ACCOUNT_ADDRESS")?)?;
        let starknet_private_key = Felt::from_hex(&std::env::var("STARKNET_PRIVATE_KEY")?)?;
        let starknet_chain_id = Felt::from_hex(
            &std::env::var("STARKNET_CHAIN_ID")
                .unwrap_or_else(|_| "0x534e5f5345504f4c4941".to_string()),
        )?;
        let starknet_factory_address = Felt::from_hex(&std::env::var("STARKNET_FACTORY_ADDRESS")?)?;

        let lit_relay_url = std::env::var("LIT_RELAY_URL")
            .unwrap_or_else(|_| "https://relay.litprotocol.com".to_string());
        let lit_cosigner_pubkey =
            std::env::var("LIT_COSIGNER_PUBKEY").unwrap_or_else(|_| "".to_string());
        let lit_api_key = std::env::var("LIT_API_KEY").unwrap_or_else(|_| "".to_string());
        let lit_action_ipfs_cid = std::env::var("LIT_ACTION_IPFS_CID")
            .unwrap_or_else(|_| "QmW7Z1g5k6v1x3y4z5a6b7c8d9e0f1g2h3i4j5k6l7m8n9".to_string());

        let rate_limit_per_hour = std::env::var("RATE_LIMIT_PER_HOUR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let listen_addr = std::env::var("LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()?;

        Ok(Self {
            rate_limit_per_hour,
            listen_addr,
            evm_rpc_url,
            evm_keeper_private_key,
            evm_factory_address,
            webhook_registry_address,
            starknet_rpc_url,
            starknet_account_address,
            starknet_private_key,
            starknet_chain_id,
            starknet_factory_address,
            lit_relay_url,
            lit_cosigner_pubkey,
            lit_api_key,
            lit_action_ipfs_cid,
        })
    }
}
