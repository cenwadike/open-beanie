use crate::models::*;
// Dual-Bucket Rate Limiter ──────────────────────────────────────────────

pub struct DualRateLimiter {
    window: Duration,
    ip_limit: u32,
    address_limit: u32,
    ip_hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    address_hits: Mutex<HashMap<String, (Instant, u32)>>,
}

impl DualRateLimiter {
    pub fn new(ip_limit: u32, address_limit: u32, window: Duration) -> Self {
        Self {
            window,
            ip_limit,
            address_limit,
            ip_hits: Mutex::new(HashMap::new()),
            address_hits: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, ip: IpAddr, merchant_address: &str) -> Result<(), &'static str> {
        let now = Instant::now();

        // 1. Check IP Bucket (protects RPC gas & DoS)
        {
            let mut ips = self.ip_hits.lock().unwrap();
            let entry = ips.entry(ip).or_insert((now, 0));
            if now.duration_since(entry.0) > self.window {
                *entry = (now, 0);
            }
            if entry.1 >= self.ip_limit {
                return Err("IP rate limit exceeded, try again later");
            }
            entry.1 += 1;
        }

        // 2. Check Merchant Address Bucket (prevents spam targeting specific accounts)
        {
            let mut addresses = self.address_hits.lock().unwrap();
            let entry = addresses
                .entry(merchant_address.to_lowercase())
                .or_insert((now, 0));
            if now.duration_since(entry.0) > self.window {
                *entry = (now, 0);
            }
            if entry.1 >= self.address_limit {
                return Err("Merchant address registration limit exceeded");
            }
            entry.1 += 1;
        }

        Ok(())
    }
}
