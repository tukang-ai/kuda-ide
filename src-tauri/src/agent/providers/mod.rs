pub mod gemini;
pub mod openai;

/// Collapses an untrusted upstream error body into a single line so a hostile
/// relay cannot smuggle multi-line prompt-injection payloads (fake tool
/// results, fake system banners) through error messages that later re-enter
/// the model context.
pub fn sanitize_single_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}
