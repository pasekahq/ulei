//! Money of a night run (ADR-0078 section 6): the cost of a call from the gateway's usage answer
//! and the route's prices, the estimate of a prompt, and the spend limit of a run.

use crate::wire::c4g::Usage;

/// USD per million tokens of the route's model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    /// Input tokens neither read from nor written to the prompt cache.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Input tokens read from the cache.
    pub cache_read: f64,
    /// Input tokens written to the cache.
    pub cache_write: f64,
}

impl Prices {
    /// The defaults of C9: a small model's price list, to be set to the route's.
    pub const DEFAULT: Self = Self {
        input: 1.0,
        output: 5.0,
        cache_read: 0.1,
        cache_write: 1.25,
    };

    /// USD of `usage`.
    #[must_use]
    pub fn cost(&self, usage: &Usage) -> f64 {
        (tokens(usage.input) * self.input
            + tokens(usage.output) * self.output
            + tokens(usage.cache_read) * self.cache_read
            + tokens(usage.cache_write) * self.cache_write)
            / 1_000_000.0
    }

    /// USD of `input` uncached input tokens.
    #[must_use]
    pub fn input_cost(&self, input: u64) -> f64 {
        tokens(input) * self.input / 1_000_000.0
    }
}

#[allow(clippy::cast_precision_loss)]
fn tokens(n: u64) -> f64 {
    n as f64
}

/// Estimated tokens of `text`: ASCII characters at 3.5 per token, any other at 2, since
/// Cyrillic text takes about two characters per token where English takes about four.
#[must_use]
pub fn estimate_tokens(text: &str) -> u64 {
    let (ascii, other) = text.chars().fold((0u64, 0u64), |(a, o), c| {
        if c.is_ascii() { (a + 1, o) } else { (a, o + 1) }
    });
    // ceil(ascii / 3.5 + other / 2) = ceil((4 * ascii + 7 * other) / 14)
    (4 * ascii + 7 * other).div_ceil(14)
}

/// Estimated output tokens of a call with `input` estimated input tokens: a quarter of the
/// input, at most `max_tokens`.
#[must_use]
pub fn estimate_output(input: u64, max_tokens: u64) -> u64 {
    input.div_ceil(4).min(max_tokens)
}

/// The spend of a run against its limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// The limit in USD.
    pub limit: f64,
    /// USD spent so far.
    pub spent: f64,
}

impl Budget {
    /// A budget of `limit` USD with nothing spent.
    #[must_use]
    pub fn new(limit: f64) -> Self {
        Self { limit, spent: 0.0 }
    }

    /// Whether a call whose estimated input costs `estimate` USD may start: the spend so far plus
    /// the estimate stays under the limit.
    #[must_use]
    pub fn allows(&self, estimate: f64) -> bool {
        self.spent + estimate < self.limit
    }

    /// Adds the cost of a finished call.
    pub fn charge(&mut self, usd: f64) {
        self.spent += usd;
    }

    /// Whether the spend has reached the limit.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.spent >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_prices_every_kind_of_token() {
        let usage = Usage {
            input: 1_000_000,
            output: 200_000,
            cache_read: 500_000,
            cache_write: 100_000,
        };
        let usd = Prices::DEFAULT.cost(&usage);
        assert!((usd - (1.0 + 1.0 + 0.05 + 0.125)).abs() < 1e-9, "{usd}");
        assert!((Prices::DEFAULT.input_cost(2_000_000) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn estimate_counts_cyrillic_denser_than_ascii() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdefg"), 2);
        assert_eq!(estimate_tokens(&"a".repeat(35)), 10);
        assert_eq!(estimate_tokens("привет"), 3);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_output(400, 8_192), 100);
        assert_eq!(estimate_output(400_000, 8_192), 8_192);
    }

    #[test]
    fn budget_stops_at_the_limit() {
        let mut budget = Budget::new(1.0);
        assert!(budget.allows(0.5));
        budget.charge(0.6);
        assert!(!budget.allows(0.4));
        assert!(budget.allows(0.3));
        assert!(!budget.exhausted());
        budget.charge(0.5);
        assert!(budget.exhausted());
        assert!(!Budget::new(0.0).allows(0.0));
    }
}
