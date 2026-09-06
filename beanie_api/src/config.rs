use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub rate_limit_per_hour: u32,
    pub listen_addr: SocketAddr,

    // Optional Lit Protocol Config
    pub lit_relay_url: String,
    pub lit_api_key: String,
    pub lit_action_ipfs_cid: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let lit_relay_url = std::env::var("LIT_RELAY_URL")
            .unwrap_or_else(|_| "https://relay.litprotocol.com".to_string());
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
            lit_relay_url,
            lit_api_key,
            lit_action_ipfs_cid,
        })
    }
}
