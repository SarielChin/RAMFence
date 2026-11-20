//! Pure logic for badram.efi, kept free of any UEFI dependency so it can be
//! unit-tested on the host (`cargo test -p badram-core`).
//!
//! - [`config`]: parse `badram.cfg`.
//! - [`range`]: physical address ranges, outward page alignment, merging.
//! - [`memmap`]: intersect configured ranges with the firmware memory map,
//!   decide what to allocate, and verify the result.
//! - [`status`]: build the `BadRamStatus` variable contents.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod config;
pub mod memmap;
pub mod memtest;
pub mod range;
pub mod status;

pub use config::{Config, MemoryType, OnError, ParseError, ParseErrorKind};
pub use memmap::{Class, MemDesc, Plan, Segment};
pub use range::{PAGE_SIZE, Range, RangeError};
