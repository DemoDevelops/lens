//! Pipeline prose. We tokenize the stream, then tokenize the tail; the tokenize
//! pass is the hot path. tokenize, tokenize, tokenize. Profile the tokenize step.
pub fn pipeline_stage() -> bool {
    true
}
