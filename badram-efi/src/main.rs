//! badram.efi — skeleton. The real boot flow (DESIGN.md §2) is implemented in phase 2.

#![no_main]
#![no_std]

use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::ABORTED;
    }
    uefi::println!("badram.efi skeleton");
    Status::SUCCESS
}
