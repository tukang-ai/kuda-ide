use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::agent::llm_client::CompletionRequest;
use crate::agent::tokenizer::Tokenizer;
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub max_requests_per_minute: u32,
    pub max_tokens_per_minute: u32,
    pub daily_budget_cents: u32, // Default: 500 ($5.00)
    pub cost_per_1k_input_tokens: f64,
    pub cost_per_1k_output_tokens: f64,
    pub burst_capacity: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        // Generous for a local dev tool so the now-wired pipeline never bricks
        // legitimate swarm runs (a single swarm = many back-to-back requests):
        // the budget is a coarse runaway-cost guard, not a tight throttle.
        Self {
            max_requests_per_minute: 300,
            max_tokens_per_minute: 20_000,
            daily_budget_cents: 10_000, // $100.00 daily runaway-cost guard
            cost_per_1k_input_tokens: 0.15,
            cost_per_1k_output_tokens: 0.60,
            burst_capacity: 5,
        }
    }
}

#[derive(Debug, Clone)]
struct TokenBucket {
    /// Available tokens (refilled at `max_tokens_per_minute / 60` per second).
    tokens: f64,
    /// Unix seconds of the last refill / consume.
    last_refill: i64,
    /// Tokens consumed in the current per-minute window (rate check).
    token_count: u32,
    request_count: u32,
    window_start: i64,
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
    buckets: Mutex<HashMap<String, TokenBucket>>,
    daily_usage: Mutex<HashMap<String, DailyUsage>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
            daily_usage: Mutex::new(HashMap::new()),
        }
    }

    /// Estimates the tokens a request will consume (input + an output share)
    /// using the app's local tokenizer so the token bucket has real input.
    fn estimate_request_tokens(request: &CompletionRequest) -> f64 {
        let input = Tokenizer::count_tokens(&request.system_prompt)
            + request
                .messages
                .iter()
                .map(|m| Tokenizer::count_tokens(&m.content))
                .sum::<usize>();
        // Reserve a flat output allowance so one huge streaming answer cannot
        // dodge the limit.
        (input as f64) + 8_000.0
    }

    pub fn check_and_consume(&self, device_hash: &str, request: &CompletionRequest) -> Result<()> {
        let now = Utc::now().timestamp();
        let today = Utc::now().date_naive().to_string();

        if let Ok(mut daily) = self.daily_usage.lock() {
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

            if usage.estimated_cost_cents >= self.config.daily_budget_cents {
                return Err(AppError::QuotaExceeded(format!(
                    "Daily budget cap of ${:.2} reached for your device",
                    self.config.daily_budget_cents as f64 / 100.0
                )));
            }
        }

        if let Ok(mut buckets) = self.buckets.lock() {
            let bucket = buckets.entry(device_hash.to_string()).or_insert_with(|| TokenBucket {
                tokens: self.config.max_tokens_per_minute as f64,
                last_refill: now,
                token_count: 0,
                request_count: 0,
                window_start: now / 60,
            });

            let current_min = now / 60;
            if bucket.window_start != current_min {
                bucket.window_start = current_min;
                bucket.request_count = 0;
                bucket.token_count = 0;
            }

            if bucket.request_count >= self.config.max_requests_per_minute {
                return Err(AppError::RateLimitExceeded(format!(
                    "Max {} requests per minute exceeded",
                    self.config.max_requests_per_minute
                )));
            }

            // Token bucket refill: rate = max_tokens_per_minute/60 per second,
            // capped at `burst_capacity × rate` so a sudden swarm burst cannot
            // hoard an infinite backlog (the old fields were never read, so the
            // tokens-per-minute limit was never actually enforced).
            let rate_per_sec = self.config.max_tokens_per_minute.max(1) as f64 / 60.0;
            let capacity = rate_per_sec * self.config.burst_capacity.max(1) as f64;
            let elapsed = now.saturating_sub(bucket.last_refill) as f64;
            if elapsed > 0.0 {
                bucket.tokens = (bucket.tokens + elapsed * rate_per_sec).min(capacity);
                bucket.last_refill = now;
            }

            let needed = Self::estimate_request_tokens(request);
            if bucket.tokens < needed {
                return Err(AppError::RateLimitExceeded(format!(
                    "Token rate limit exceeded: ~{:.0} tokens needed but only ~{:.0} \
                     available. Try again in a moment.",
                    needed, bucket.tokens
                )));
            }
            bucket.tokens -= needed;
            bucket.token_count += needed as u32;
            bucket.request_count += 1;
        }

        Ok(())
    }

    pub fn record_usage(&self, device_hash: &str, input_tokens: u64, output_tokens: u64) {
        if let Ok(mut daily) = self.daily_usage.lock() {
            if let Some(usage) = daily.get_mut(device_hash) {
                usage.total_input_tokens += input_tokens;
                usage.total_output_tokens += output_tokens;
                usage.total_requests += 1;

                let input_cost = (input_tokens as f64 / 1000.0) * self.config.cost_per_1k_input_tokens;
                let output_cost = (output_tokens as f64 / 1000.0) * self.config.cost_per_1k_output_tokens;
                usage.estimated_cost_cents += ((input_cost + output_cost) * 100.0) as u32;
            }
        }
    }

    pub fn get_usage_stats(&self, device_hash: &str) -> Option<DailyUsage> {
        self.daily_usage.lock().ok()?.get(device_hash).cloned()
    }
}
