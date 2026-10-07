//! Observability track shared by every milestone.
//!
//! - [`init_tracing`]: `tracing` subscriber configured from `RUST_LOG`.
//! - [`fields`]: canonical span and metric field names.
//! - [`RequestTimings`]: per-request token timestamps producing TTFT, ITL, and TPOT.
//!
//! Dynamo counterpart: `lib/llm/src/request_trace` (including `otel_sink.rs`).

use std::time::{Duration, Instant};

/// Canonical field names, so every crate emits the same keys.
pub mod fields {
    pub const REQUEST_ID: &str = "request_id";
    pub const TENANT: &str = "tenant";
    pub const WORKER_ID: &str = "worker_id";
    pub const TTFT_MS: &str = "ttft_ms";
    pub const ITL_MS: &str = "itl_ms";
    pub const TPOT_MS: &str = "tpot_ms";
    pub const CACHE_HIT_BLOCKS: &str = "cache_hit_blocks";
    pub const PROMPT_BLOCKS: &str = "prompt_blocks";
    pub const QUEUE_DEPTH: &str = "queue_depth";
    pub const BLOCKS_IN_USE: &str = "blocks_in_use";
    pub const BLOCKS_TOTAL: &str = "blocks_total";
}

/// Installs a global `tracing` subscriber filtered by `RUST_LOG` (default `info`).
/// Safe to call more than once; later calls are no-ops.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Records when a request arrived and when each output token was emitted.
#[derive(Debug, Clone)]
pub struct RequestTimings {
    arrived: Instant,
    token_times: Vec<Instant>,
}

impl RequestTimings {
    pub fn start(arrived: Instant) -> Self {
        Self {
            arrived,
            token_times: Vec::new(),
        }
    }

    /// Records one emitted output token. Timestamps must be non-decreasing.
    pub fn record_token(&mut self, at: Instant) {
        debug_assert!(self.token_times.last().is_none_or(|&last| at >= last));
        self.token_times.push(at);
    }

    pub fn output_tokens(&self) -> usize {
        self.token_times.len()
    }

    /// Time to first token.
    pub fn ttft(&self) -> Option<Duration> {
        self.token_times.first().map(|&t| t - self.arrived)
    }

    /// Inter-token latencies: one value per gap between consecutive output tokens.
    pub fn itl(&self) -> Vec<Duration> {
        self.token_times.windows(2).map(|w| w[1] - w[0]).collect()
    }

    /// Time per output token: (last token - first token) / (tokens - 1).
    pub fn tpot(&self) -> Option<Duration> {
        let n = u32::try_from(self.token_times.len()).ok()?;
        match (self.token_times.first(), self.token_times.last()) {
            (Some(&first), Some(&last)) if n > 1 => Some((last - first) / (n - 1)),
            _ => None,
        }
    }

    /// Emits one `tracing` event carrying the summary fields.
    pub fn emit(&self, request_id: &str) {
        let ms = |d: Option<Duration>| d.map_or(f64::NAN, |d| d.as_secs_f64() * 1e3);
        let itl = self.itl();
        let itl_max = itl.iter().max().copied();
        tracing::info!(
            request_id,
            ttft_ms = ms(self.ttft()),
            tpot_ms = ms(self.tpot()),
            itl_max_ms = ms(itl_max),
            output_tokens = self.output_tokens(),
            "request complete"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn build(arrival_to_first_ms: u64, gaps_ms: &[u64]) -> RequestTimings {
        let t0 = Instant::now();
        let mut rt = RequestTimings::start(t0);
        let mut t = t0 + Duration::from_millis(arrival_to_first_ms);
        rt.record_token(t);
        for &g in gaps_ms {
            t += Duration::from_millis(g);
            rt.record_token(t);
        }
        rt
    }

    #[test]
    fn empty_request_has_no_metrics() {
        let rt = RequestTimings::start(Instant::now());
        assert_eq!(rt.ttft(), None);
        assert_eq!(rt.tpot(), None);
        assert!(rt.itl().is_empty());
    }

    #[test]
    fn single_token_has_ttft_but_no_tpot() {
        let rt = build(120, &[]);
        assert_eq!(rt.ttft(), Some(Duration::from_millis(120)));
        assert_eq!(rt.tpot(), None);
    }

    #[test]
    fn stall_shows_in_itl_but_averages_out_in_tpot() {
        let rt = build(100, &[10, 10, 200, 10]);
        assert_eq!(rt.itl().iter().max(), Some(&Duration::from_millis(200)));
        assert_eq!(
            rt.tpot(),
            Some(Duration::from_millis(57) + Duration::from_micros(500))
        );
    }

    proptest! {
        #[test]
        fn tpot_equals_mean_itl(first in 0u64..1_000, gaps in prop::collection::vec(0u64..500, 1..64)) {
            let rt = build(first, &gaps);
            let sum: Duration = rt.itl().iter().sum();
            let n = u32::try_from(gaps.len()).expect("small");
            prop_assert_eq!(rt.tpot(), Some(sum / n));
        }
    }
}
