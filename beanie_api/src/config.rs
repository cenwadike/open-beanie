use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub rate_limit_per_hour: u32,
    pub listen_addr: SocketAddr,

    pub rp_id: String,
    pub rp_origin: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let rate_limit_per_hour = std::env::var("RATE_LIMIT_PER_HOUR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let listen_addr = std::env::var("LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()?;

        let rp_id = std::env::var("RP_ID").unwrap_or_else(|_| "".to_string());
        let rp_origin = std::env::var("RP_ORIGIN").unwrap_or_else(|_| "".to_string());

        Ok(Self {
            rate_limit_per_hour,
            listen_addr,
            rp_id,
            rp_origin,
        })
    }
}
