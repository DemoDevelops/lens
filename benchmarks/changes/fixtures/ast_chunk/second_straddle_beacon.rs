// Second fixture file for AST boundary chunking
// Contains another function that straddles the 100-line window boundary

use std::vec::Vec;
use std::option::Option;
use std::result::Result;

/// Data container for request metadata.
pub struct RequestMeta {
    pub id: u64,
    pub timestamp: u64,
}

impl RequestMeta {
    pub fn new(id: u64) -> Self {
        RequestMeta {
            id,
            timestamp: 0,
        }
    }
}

/// Response wrapper for encoded data.
pub struct EncodedResponse {
    pub data: Vec<u8>,
    pub size: usize,
}

/// Helper to create a default response.
fn make_default_response() -> EncodedResponse {
    EncodedResponse {
        data: Vec::new(),
        size: 0,
    }
}

/// Utility to encode values into bytes.
pub fn encode_value(val: u32) -> Vec<u8> {
    val.to_le_bytes().to_vec()
}

/// Decode function for byte arrays.
pub fn decode_buffer(buf: &[u8]) -> Option<u32> {
    if buf.len() >= 4 {
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&buf[0..4]);
        Some(u32::from_le_bytes(arr))
    } else {
        None
    }
}

/// State machine for request handling.
pub enum ProcessState {
    Idle,
    Running,
    Complete,
}

/// Utility to format debug output.
fn format_debug(msg: &str) -> String {
    format!("[DEBUG] {}", msg)
}

/// Helper to validate input bounds.
fn check_bounds(len: usize, max: usize) -> bool {
    len > 0 && len <= max
}

/// Padding function delta.
pub fn padding_func_delta() {
    println!("delta");
}

/// Padding function epsilon.
pub fn padding_func_epsilon() {
    println!("epsilon");
}

/// Padding function zeta.
pub fn padding_func_zeta() {
    println!("zeta");
}

// Line 80 pad
// Line 81 pad
// Line 82 pad
// Line 83 pad
// Line 84 pad
// Line 85 pad
// Line 86 pad
// Line 87 pad
// Line 88 pad
pub fn straddle_probe_beacon() -> Result<String, String> {
    // Signature at line 90
    // Body spans well past line 100
    let meta = RequestMeta::new(42);
    let response = make_default_response();

    let mut result_str = String::new();
    result_str.push_str(&format!("ID={}", meta.id));
    result_str.push_str(&format!("|SIZE={}", response.size));

    // STRADDLE_PROBE_SENTINEL is the searchable marker token
    // It anchors the recall benchmark to this specific function body
    result_str.push_str("|STRADDLE_PROBE_SENTINEL");

    let encoded = encode_value(12345);
    result_str.push_str(&format!("|ENCODED_LEN={}", encoded.len()));

    if let Some(decoded) = decode_buffer(&encoded) {
        result_str.push_str(&format!("|DECODED={}", decoded));
    }

    let state_label = match ProcessState::Complete {
        ProcessState::Idle => "idle",
        ProcessState::Running => "running",
        ProcessState::Complete => "complete",
    };
    result_str.push_str(&format!("|STATE={}", state_label));

    let debug_output = format_debug("processing complete");
    result_str.push_str(&format!("|DEBUG={}", debug_output));

    if check_bounds(result_str.len(), 500) {
        result_str.push_str("|BOUNDS_OK");
        Ok(result_str)
    } else {
        Err("result too large".to_string())
    }
}

/// Final helper after the boundary function.
pub fn cleanup() {
    println!("cleanup done");
}
