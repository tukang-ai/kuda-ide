use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::agent::llm_client::CompletionRequest;
use crate::error::Result;

/// TANPA limit lokal. Kuota pemakaian (points/tokens/requests per plan)
/// sepenuhnya diawasi oleh server (hub). RateLimiter di IDE hanya MENCATAT
/// pemakaian untuk statistik UI — tidak pernah memblokir request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub cost_per_1k_input_tokens: f64,
    pub cost_per_1k_output_tokens: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            cost_per_1k_input_tokens: 0.15,
            cost_per_1k_output_tokens: 0.60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyUsage {
    pub date: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_requests: u32,
    pub estimated_cost_cents: u32,
}

pub struct RateLimiter {
    config: RateLimitConfig,
    daily_usage: Mutex<HashMap<String, DailyUsage>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            daily_usage: Mutex::new(HashMap::new()),
        }
    }

    /// No-op: tidak ada limit lokal; server (hub) yang menegakkan kuota.
    pub fn check_and_consume(&self, _device_hash: &str, _request: &CompletionRequest) -> Result<()> {
        Ok(())
    }

    pub fn record_usage(&self, device_hash: &str, input_tokens: u64, output_tokens: u64) {
        if let Ok(mut daily) = self.daily_usage.lock() {
            let today = Utc::now().date_naive().to_string();
            let usage = daily.entry(device_hash.to_string()).or_insert_with(|| DailyUsage {
                date: today.clone(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_requests: 0,
                estimated_cost_cents: 0,
            });

            if usage.date != today {
                usage.date = today;
                usage.total_input_tokens = 0;
                usage.total_output_tokens = 0;
                usage.total_requests = 0;
                usage.estimated_cost_cents = 0;
            }

            usage.total_input_tokens += input_tokens;
            usage.total_output_tokens += output_tokens;
            usage.total_requests += 1;

            let input_cost = (input_tokens as f64 / 1000.0) * self.config.cost_per_1k_input_tokens;
            let output_cost = (output_tokens as f64 / 1000.0) * self.config.cost_per_1k_output_tokens;
            usage.estimated_cost_cents += ((input_cost + output_cost) * 100.0) as u32;
        }
    }

    pub fn get_usage_stats(&self, device_hash: &str) -> Option<DailyUsage> {
        self.daily_usage.lock().ok()?.get(device_hash).cloned()
    }
}
