pub fn decode_frame(frame: &[u8]) -> u32 {
    parse_shard_header(frame)
}
