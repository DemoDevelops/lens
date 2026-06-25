//! Wire prose. We marshal the header, then marshal the body; marshal before send.
//! marshal, marshal, marshal. The marshal buffer and the marshal order matter.
pub fn wire_stage() -> bool {
    true
}
