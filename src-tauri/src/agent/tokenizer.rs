use tiktoken_rs::cl100k_base;

pub struct Tokenizer;

impl Tokenizer {
    /// Estimates token count for text using cl100k_base offline tokenizer
    pub fn count_tokens(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        match cl100k_base() {
            Ok(bpe) => bpe.encode_with_special_tokens(text).len(),
            Err(_) => text.len() / 4, // Fallback rule-of-thumb (~4 chars/token)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_tokens() {
        let text = "fn main() { println!(\"Hello World\"); }";
        let count = Tokenizer::count_tokens(text);
        assert!(count > 0);
    }
}
