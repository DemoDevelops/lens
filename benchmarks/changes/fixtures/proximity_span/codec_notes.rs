// parse the input frame stream cleanly on entry.
// parse the next record without any extra delay.
// parse incremental delta updates as they arrive.
// parse the trailing padding bytes at the very end.
// parse the framing markers up front before bodies.
// parse each block boundary carefully in turn.
pub fn codec() -> usize {
    0
}
// The version field is little endian encoded.
// The length precedes the body region always.
// The trailer holds a final marker byte value.
// header precedes the input region in this layout.
// header carries the schema number for the block.
// header includes a length prefix field as well.
// header stores the content type code value too.
// header has a reserved flag set early on always.
// header ends with a magic value byte at the close.
