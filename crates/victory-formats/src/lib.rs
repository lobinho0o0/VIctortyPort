//! Platform-level data formats, shared by every Xbox Series title.
//!
//! Each module is pure (bytes in, bytes out, no I/O):
//!
//! * [`texture`]: the GPU's tiled texture layouts and copies to and from
//!   linear rows.
//! * [`bcpack`]: BCPack, the packing of BC1-BC7 block streams that
//!   hardware decompression unpacks.
//! * [`xma`]: XMA2, the hardware audio codec.
//!
//! Opus streams and the zlib streams of hardware decompression are decoded
//! in `victory-hle` with `opus-pure` and `miniz_oxide`, not here.

pub mod bcpack;
pub mod texture;
pub mod xma;
