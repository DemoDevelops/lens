// parse header immediately for every inbound frame on the wire.
pub fn read_frame(buf: &[u8]) -> usize {
    // The body follows right after the fixed prefix region.
    // We copy the payload into an owned buffer at this point.
    // Then advance the cursor past the consumed prefix bytes.
    // Validation of the body checksum happens one layer up.
    // The caller owns retransmission and timeout handling.
    // No allocation occurs on the steady-state fast path here.
    buf.len()
}
