//! Machine id of this PC and memory configuration (see
//! `ramfence_core::machine`), from the firmware's SMBIOS table and memory map.

use ramfence_core::{machine, memmap};
use uefi::table::cfg::ConfigTableEntry;

use crate::reserve;

/// Upper bound for the SMBIOS table length we are willing to read.
const MAX_TABLE: usize = 1 << 20;

pub fn id() -> u64 {
    let ram = reserve::read_map().ok().and_then(|m| memmap::ram_extent(&m));
    machine::machine_id(smbios_table(), ram)
}

fn smbios_table() -> Option<&'static [u8]> {
    let entry = uefi::system::with_config_table(|tables| {
        let find = |guid| tables.iter().find(|e| e.guid == guid).map(|e| e.address as usize);
        find(ConfigTableEntry::SMBIOS3_GUID).or_else(|| find(ConfigTableEntry::SMBIOS_GUID))
    })?;
    if entry == 0 {
        return None;
    }
    // SAFETY: the firmware publishes the SMBIOS entry point and table in
    // memory that stays mapped (identity-mapped) during boot services. We
    // read the 5-byte anchor first, then exactly the entry point's length,
    // then the table length it declares (bounded by MAX_TABLE).
    unsafe {
        let anchor = core::slice::from_raw_parts(entry as *const u8, 5);
        let len = machine::entry_point_len(anchor)?;
        let ep = core::slice::from_raw_parts(entry as *const u8, len);
        let (addr, table_len) = machine::table_location(ep)?;
        if addr == 0 || table_len == 0 || table_len > MAX_TABLE {
            return None;
        }
        Some(core::slice::from_raw_parts(addr as *const u8, table_len))
    }
}
