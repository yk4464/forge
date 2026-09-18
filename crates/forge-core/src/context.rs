use crate::config::ContextConfig;
use crate::history::History;
use crate::message::Message;

/// Byte-based token heuristics (codex-rs convention): ~4 bytes per token.
pub const APPROX_BYTES_PER_TOKEN: usize = 4;

pub fn approx_token_count(text: &str) -> i64 {
    (text.len().div_ceil(APPROX_BYTES_PER_TOKEN)) as i64
}

pub fn approx_tokens_for_message(msg: &Message) -> i64 {
    approx_token_count(&serde_json_like_bytes(msg))
}

/// Slightly pessimistic estimate: JSON-escape cost is approximated by +10%.
fn serde_json_like_bytes(msg: &Message) -> String {
    let n = msg.content_bytes();
    let approx = n + n / 10;
    String::from_utf8(vec![b'x'; approx]).unwrap_or_default()
}

/// Running token accounting. Real usage from API responses is authoritative
/// for what the model has already seen; local estimates cover items added
/// since the last successful response. We never overwrite the cumulative
/// fields (the codex fill_to_context_window mutation bug) — overflow is
/// tracked in a separate flag.
#[derive(Debug, Clone, Default)]
pub struct TokenAccounting {
    /// Cumulative total from the last API response (authoritative).
    last_response_total: i64,
    /// Sum of estimates for items appended after that response.
    local_estimate: i64,
    /// Set when we believe usage reached/exceeded the window.
    overflow: bool,
    /// True once at least one API usage value has been recorded.
    has_real_usage: bool,
}

impl TokenAccounting {
    /// Record real usage from an API response. Zero-valued totals (a
    /// gateway dropping the usage fields) are ignored so a single bad
    /// chunk cannot re-anchor the estimate to 0 and suppress compaction.
    pub fn record_response_usage(&mut self, usage: crate::message::Usage) {
        if usage.total_tokens <= 0 {
            return;
        }
        self.last_response_total = usage.total_tokens;
        self.local_estimate = 0;
        self.has_real_usage = true;
        self.overflow = false;
    }

    pub fn record_local_item(&mut self, msg: &Message) {
        self.local_estimate += approx_tokens_for_message(msg);
    }

    pub fn current_estimate(&self) -> i64 {
        self.last_response_total + self.local_estimate
    }

    pub fn has_real_usage(&self) -> bool {
        self.has_real_usage
    }

    pub fn mark_overflow(&mut self) {
        self.overflow = true;
    }

    /// Effective limit: explicit config value, else floor(window * 0.90),
    /// computed against the effective (95%) window — avoiding the codex
    /// 94.7%-of-usable-window mismatch. The threshold also reserves room
    /// for the completion itself: history may only grow up to
    /// window - max_tokens, otherwise a request can overflow the raw
    /// window before the compact threshold is ever reached.
    pub fn auto_compact_limit(cfg: &ContextConfig) -> i64 {
        let effective_window = (cfg.context_window as f64 * 0.95).floor() as i64;
        // Reserve output budget against the RAW window (what the API
        // actually enforces), then keep the 95% safety margin.
        let input_headroom = cfg
            .context_window
            .saturating_sub(cfg.max_output_tokens as i64);
        let bounded_window = effective_window.min(input_headroom).max(0);
        let default = (bounded_window as f64 * 0.90).floor() as i64;
        match cfg.auto_compact_token_limit {
            Some(v) => v.min(default),
            None => default,
        }
    }

    /// Hard ceiling: effective window itself (95% of raw).
    pub fn hard_limit(cfg: &ContextConfig) -> i64 {
        (cfg.context_window as f64 * 0.95).floor() as i64
    }

    /// Should auto-compaction fire?
    pub fn should_compact(&self, cfg: &ContextConfig) -> bool {
        let used = self.current_estimate();
        used >= Self::auto_compact_limit(cfg) || self.overflow
    }
}

/// Owns history + accounting. Future home for per-item truncation policies.
pub struct ContextManager {
    pub history: History,
    pub accounting: TokenAccounting,
    cfg: ContextConfig,
    /// Completion budget reserved when deriving the compact threshold.
    max_output_tokens: i64,
}

impl ContextManager {
    pub fn new(cfg: ContextConfig) -> Self {
        Self {
            history: History::new(),
            accounting: TokenAccounting::default(),
            cfg,
            max_output_tokens: 8192,
        }
    }

    /// Sync the completion budget used by the compaction threshold with
    /// the request budget (call after construction, before the first turn).
    pub fn set_max_output_tokens(&mut self, tokens: i64) {
        self.max_output_tokens = tokens.max(0);
    }

    pub fn config(&self) -> &ContextConfig {
        &self.cfg
    }

    pub fn push(&mut self, msg: Message) {
        self.accounting.record_local_item(&msg);
        self.history.push(msg);
    }

    pub fn record_usage(&mut self, usage: crate::message::Usage) {
        self.accounting.record_response_usage(usage);
    }

    /// Compact check after any local insertion (mid-turn safe).
    pub fn should_compact(&self) -> bool {
        self.accounting.should_compact(&self.cfg)
    }

    pub fn token_status(&self) -> (i64, i64) {
        (
            self.accounting.current_estimate(),
            TokenAccounting::auto_compact_limit(&self.cfg),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(window: i64, limit: Option<i64>) -> ContextConfig {
        ContextConfig {
            context_window: window,
            max_output_tokens: 8192,
            auto_compact_token_limit: limit,
            tool_output_max_bytes: 1 << 20,
            shell_path: None,
            shell_timeout_secs: 120,
        }
    }

    #[test]
    fn threshold_derivation() {
        // effective = 200000*0.95 = 190000; reserved input = 200000-8192;
        // bounded window = 190000; default = *0.90 = 171000
        assert_eq!(TokenAccounting::auto_compact_limit(&cfg(200_000, None)), 171_000);
        // explicit lower limit wins
        assert_eq!(TokenAccounting::auto_compact_limit(&cfg(200_000, Some(50_000))), 50_000);
        // explicit limit above default is clamped
        assert_eq!(TokenAccounting::auto_compact_limit(&cfg(200_000, Some(190_000))), 171_000);
    }

    #[test]
    fn threshold_reserves_output_budget() {
        // 32k window + 8k output: raw reserve leaves 24576 input tokens;
        // threshold = min(31232, 24576) * 0.90 = 22118. The old formula
        // (28016) let history grow into a region where prompt+completion
        // already overflowed the raw window.
        let mut c = cfg(32_768, None);
        c.max_output_tokens = 8192;
        assert_eq!(TokenAccounting::auto_compact_limit(&c), 22_118);
    }

    #[test]
    fn zero_usage_is_ignored() {
        let mut t = TokenAccounting::default();
        t.record_response_usage(crate::message::Usage {
            input_tokens: 1000,
            output_tokens: 100,
            total_tokens: 1100,
        });
        // A gateway dropping usage fields must not re-anchor to 0.
        t.record_response_usage(crate::message::Usage::default());
        assert_eq!(t.current_estimate(), 1100);
        assert!(t.has_real_usage());
    }

    #[test]
    fn accounting_accumulates() {
        let mut t = TokenAccounting::default();
        t.record_local_item(&Message::user(&"x".repeat(400)));
        assert_eq!(t.current_estimate(), 110); // 400/4 ceil + 10%
        t.record_response_usage(crate::message::Usage {
            input_tokens: 1000,
            output_tokens: 100,
            total_tokens: 1100,
        });
        assert_eq!(t.current_estimate(), 1100);
        t.record_local_item(&Message::user(&"y".repeat(400)));
        assert_eq!(t.current_estimate(), 1210);
    }
}
