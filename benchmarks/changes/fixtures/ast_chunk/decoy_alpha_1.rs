// Decoy file 1 for gate c42: repeats the term below, never the other one.
// occurrence one: alpha
// occurrence two: alpha

fn noise_marker_1() -> u32 {
    let count = 1;
    // occurrence three: alpha
    count
}

/// Second helper in decoy 1, keeps the file a valid small module.
pub fn noise_helper_1(x: u32) -> u32 {
    x + 1
}

