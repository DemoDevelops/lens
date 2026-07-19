// Decoy file 2 for gate c42: repeats the term below, never the other one.
// occurrence one: bravo
// occurrence two: bravo

fn noise_marker_2() -> u32 {
    let count = 2;
    // occurrence three: bravo
    count
}

/// Second helper in decoy 2, keeps the file a valid small module.
pub fn noise_helper_2(x: u32) -> u32 {
    x + 1
}

