//! Single weak entry point: hub `stream_pipe` (low-lexical) is called by just ONE
//! matched handler. Tests whether one seed source gives personalized PR enough
//! mass to lift the hub past five strong "stream filter" decoys.
pub fn stream_pipe(id: u64) -> u64 { id << 1 }
pub fn stream_tap_1(id: u64) -> u64 { stream_pipe(id) }
