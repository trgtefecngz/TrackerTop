use std::sync::OnceLock;
use tiktoken_rs::{CoreBPE, cl100k_base};

static BPE: OnceLock<CoreBPE> = OnceLock::new();

fn get_bpe() -> &'static CoreBPE {
    BPE.get_or_init(|| cl100k_base().expect("bundled cl100k_base BPE vocabulary must load offline"))
}

/// Returns exact offline BPE token count (`cl100k_base`, close match to modern LLM tokenizers).
pub fn count_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    get_bpe().encode_with_special_tokens(text).len()
}

/// Truncates text cleanly on line boundaries to fit within `max_tokens`.
/// Appends a short marker if lines were omitted.
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> (String, usize, bool) {
    let total = count_tokens(text);
    if max_tokens == 0 || total <= max_tokens {
        return (text.to_string(), total, false);
    }

    let reserve = 12usize; // room for truncation notice
    let target = max_tokens.saturating_sub(reserve);
    let mut out = String::new();
    let mut used = 0usize;
    let mut kept = 0usize;
    let lines: Vec<&str> = text.lines().collect();

    for line in &lines {
        let line_toks = count_tokens(line) + 1;
        if used + line_toks > target && kept > 0 {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        used += line_toks;
        kept += 1;
    }

    let omitted = lines.len().saturating_sub(kept);
    if omitted > 0 {
        out.push_str(&format!(
            "\n... [+{omitted} lines omitted to fit {max_tokens} tok budget]"
        ));
    }
    let final_toks = count_tokens(&out);
    (out, final_toks, omitted > 0)
}

/// Approximate input-token cost saved in USD for popular frontier coding models.
/// - Claude Sonnet tier: ~$3.00 / 1M input tokens
/// - Claude Opus tier:   ~$15.00 / 1M input tokens
pub fn estimate_usd_saved(tokens_saved: usize) -> (f64, f64) {
    let millions = tokens_saved as f64 / 1_000_000.0;
    (millions * 3.0, millions * 15.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_truncates_tokens() {
        let sample = "fn hello() {\n    println!(\"world\");\n}\n".repeat(40);
        let full = count_tokens(&sample);
        assert!(full > 100);

        let (cut, cut_toks, truncated) = truncate_to_tokens(&sample, 50);
        assert!(truncated);
        assert!(cut_toks <= 60);
        assert!(cut.contains("lines omitted"));
    }
}
