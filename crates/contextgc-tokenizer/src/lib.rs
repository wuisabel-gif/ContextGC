//! Token counting abstraction.
//!
//! Provides exact counters when a tokenizer is available and a conservative
//! approximate fallback otherwise.

pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> u64;
    fn exact(&self) -> bool;
}

/// Approximate token counter using Unicode word boundaries.
pub struct ApproximateCounter;

impl TokenCounter for ApproximateCounter {
    fn count(&self, text: &str) -> u64 {
        use unicode_segmentation::UnicodeSegmentation;
        text.split_word_bounds()
            .filter(|s| !s.trim().is_empty())
            .count() as u64
    }

    fn exact(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approximate_counts_words() {
        let counter = ApproximateCounter;
        assert_eq!(counter.count("hello world"), 2);
        assert_eq!(counter.count(""), 0);
    }
}
