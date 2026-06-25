//! Rate limiting.
pub fn throttle(rate: usize) -> usize {
    rate / 2
}
