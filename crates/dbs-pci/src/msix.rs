// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::cmp::min;

#[cfg(feature = "vm-snapshot")]
use versionize::{VersionMap, Versionize, VersionizeResult};
#[cfg(feature = "vm-snapshot")]
use versionize_derive::Versionize;
use vm_device::interrupt::{
    DeviceInterruptManager, DeviceInterruptMode, InterruptIndex, InterruptManager,
};
use vm_memory::ByteValued;

use crate::configuration::{PciCapability, PciCapabilityID};

#[cfg(feature = "vm-snapshot")]
#[path = "msix_persist.rs"]
pub mod persist;

const MAX_MSIX_VECTORS_PER_DEVICE: u16 = 2048;
const FUNCTION_MASK_BIT: u8 = 14;
const MSIX_ENABLE_BIT: u8 = 15;

pub const FUNCTION_MASK_MASK: u16 = (1 << FUNCTION_MASK_BIT) as u16;
pub const MSIX_ENABLE_MASK: u16 = (1 << MSIX_ENABLE_BIT) as u16;
pub const MSIX_TABLE_ENTRY_SIZE: usize = 16;
pub const MSIX_TABLE_ENTRIES_MODULO: u64 = 16;

/// Struct to maintain information for PCI Message Signalled Interrupt Extended Capability.
///
/// This struct is the shadow copy of the PCI MSI-x capability. Guest device drivers read from/write
/// to this struct. There's another struct MsixState, which maintains the working state about the
/// PCI MSI-x controller.
#[repr(packed)]
#[derive(Clone, Copy, Default, PartialEq)]
pub struct MsixCap {
    // Capability ID
    pub cap_id: u8,
    // Offset of next capability structure
    pub cap_next: u8,
    // Message Control Register
    //   10-0:  MSI-X Table size
    //   13-11: Reserved
    //   14:    Mask. Mask all MSI-X when set.
    //   15:    Enable. Enable all MSI-X when set.
    pub msg_ctl: u16,
    // Table. Contains the offset and the BAR indicator (BIR)
    //   2-0:  Table BAR indicator (BIR). Can be 0 to 5.
    //   31-3: Table offset in the BAR pointed by the BIR.
    pub table: u32,
    // Pending Bit Array. Contains the offset and the BAR indicator (BIR)
    //   2-0:  PBA BAR indicator (BIR). Can be 0 to 5.
    //   31-3: PBA offset in the BAR pointed by the BIR.
    pub pba: u32,
}

// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
unsafe impl ByteValued for MsixCap {}

impl MsixCap {
    pub fn new(
        table_pci_bar: u8,
        table_size: u16,
        table_off: u32,
        pba_pci_bar: u8,
        pba_off: u32,
    ) -> Self {
        assert!(table_size < MAX_MSIX_VECTORS_PER_DEVICE);

        // Set the table size and enable MSI-X.
        let msg_ctl: u16 = table_size - 1;

        MsixCap {
            cap_id: PciCapabilityID::MSIX as u8,
            cap_next: 0,
            msg_ctl,
            table: (table_off & 0xffff_fff8u32) | u32::from(table_pci_bar & 0x7u8),
            pba: (pba_off & 0xffff_fff8u32) | u32::from(pba_pci_bar & 0x7u8),
        }
    }

    pub fn set_msg_ctl(&mut self, data: u16) {
        self.msg_ctl = (self.msg_ctl & !(FUNCTION_MASK_MASK | MSIX_ENABLE_MASK))
            | (data & (FUNCTION_MASK_MASK | MSIX_ENABLE_MASK));
    }

    pub fn masked(&self) -> bool {
        (self.msg_ctl >> FUNCTION_MASK_BIT) & 0x1 == 0x1
    }

    pub fn enabled(&self) -> bool {
        (self.msg_ctl >> MSIX_ENABLE_BIT) & 0x1 == 0x1
    }

    pub fn table_offset(&self) -> u32 {
        self.table & 0xffff_fff8
    }

    pub fn pba_offset(&self) -> u32 {
        self.pba & 0xffff_fff8
    }

    pub fn table_bir(&self) -> u32 {
        self.table & 0x7
    }

    pub fn pba_bir(&self) -> u32 {
        self.pba & 0x7
    }

    pub fn table_size(&self) -> u16 {
        (self.msg_ctl & 0x7ff) + 1
    }
}

impl PciCapability for MsixCap {
    fn len(&self) -> usize {
        0xc
    }

    fn set_next_cap(&mut self, next: u8) {
        self.cap_next = next;
    }

    fn read_u8(&mut self, offset: usize) -> u8 {
        if offset < self.len() {
            self.as_slice()[offset]
        } else {
            0xff
        }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        if offset == 3 {
            self.msg_ctl = (self.msg_ctl & !(FUNCTION_MASK_MASK | MSIX_ENABLE_MASK))
                | (((value as u16) << 8) & (FUNCTION_MASK_MASK | MSIX_ENABLE_MASK));
        }
    }

    fn pci_capability_type(&self) -> PciCapabilityID {
        PciCapabilityID::MSIX
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "vm-snapshot", derive(Versionize))]
pub struct MsixTableEntry {
    pub msg_addr_lo: u32,
    pub msg_addr_hi: u32,
    pub msg_data: u32,
    pub vector_ctl: u32,
}

impl MsixTableEntry {
    pub fn masked(&self) -> bool {
        self.vector_ctl & 0x1 == 0x1
    }
}

impl Default for MsixTableEntry {
    fn default() -> Self {
        MsixTableEntry {
            msg_addr_lo: 0,
            msg_addr_hi: 0,
            msg_data: 0,
            vector_ctl: 0,
        }
    }
}

// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
// It works only for little endian platforms, but should be acceptable.
unsafe impl ByteValued for MsixTableEntry {}

#[derive(Debug, PartialEq, Clone)]
#[cfg_attr(feature = "vm-snapshot", derive(Versionize))]
pub struct MsixState {
    pub table_entries: Vec<MsixTableEntry>,
    masked: bool,
    enabled: bool,
}

impl MsixState {
    pub fn new(msix_vectors: u16) -> Self {
        assert!(msix_vectors <= MAX_MSIX_VECTORS_PER_DEVICE);

        let mut table_entries: Vec<MsixTableEntry> = Vec::new();
        table_entries.resize_with(msix_vectors as usize, Default::default);

        MsixState {
            table_entries,
            masked: false,
            enabled: false,
        }
    }

    pub fn masked(&self) -> bool {
        self.masked
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_msg_ctl<I: InterruptManager>(
        &mut self,
        reg: u16,
        intr_mgr: &mut DeviceInterruptManager<I>,
    ) -> std::result::Result<(), std::io::Error> {
        let new_masked = reg & FUNCTION_MASK_MASK != 0;
        let new_enabled = reg & MSIX_ENABLE_MASK != 0;

        // Nothing changed.
        if self.enabled == new_enabled && self.masked == new_masked {
            return Ok(());
        }

        match (self.enabled, new_enabled) {
            (false, true) => {
                intr_mgr.reset()?;
                intr_mgr.set_working_mode(DeviceInterruptMode::PciMsixIrq)?;
                for (idx, vector) in self.table_entries.iter().enumerate() {
                    intr_mgr.set_msi_high_address(idx as InterruptIndex, vector.msg_addr_hi)?;
                    intr_mgr.set_msi_low_address(idx as InterruptIndex, vector.msg_addr_lo)?;
                    intr_mgr.set_msi_data(idx as InterruptIndex, vector.msg_data)?;
                    #[cfg(target_arch = "aarch64")]
                    {
                        intr_mgr.set_msi_device_id(idx as InterruptIndex)?;
                    }
                }
                intr_mgr.enable()?;

                // Safe to unwrap() because we have just enabled interrupt successfully.
                let group = intr_mgr.get_group().unwrap();
                for (idx, vector) in self.table_entries.iter().enumerate() {
                    if new_masked || vector.masked() {
                        group.mask(idx as InterruptIndex)?;
                    }
                }
            }

            (true, false) => {
                intr_mgr.reset()?;
            }

            (true, true) => {
                // Safe to unwrap() because we are in enabled state.
                let group = intr_mgr.get_group().unwrap();
                if self.masked && !new_masked {
                    for (idx, vector) in self.table_entries.iter().enumerate() {
                        if !vector.masked() {
                            group.unmask(idx as InterruptIndex)?;
                        }
                    }
                } else if !self.masked && new_masked {
                    for (idx, vector) in self.table_entries.iter().enumerate() {
                        if !vector.masked() {
                            group.mask(idx as InterruptIndex)?;
                        }
                    }
                }
            }

            (false, false) => {}
        }

        self.enabled = new_enabled;
        self.masked = new_masked;

        Ok(())
    }

    #[cfg(target_endian = "little")]
    pub fn read_table(&self, offset: u64, data: &mut [u8]) {
        let index: usize = (offset / MSIX_TABLE_ENTRIES_MODULO) as usize;
        let modulo_offset = (offset % MSIX_TABLE_ENTRIES_MODULO) as usize;

        assert!(data.len() <= 8);
        if modulo_offset + data.len() <= MSIX_TABLE_ENTRIES_MODULO as usize {
            let config = &self.table_entries[index];
            data.copy_from_slice(&config.as_slice()[modulo_offset..modulo_offset + data.len()]);
        } else {
            debug!("invalid data length");
            for ptr in data.iter_mut() {
                *ptr = 0xffu8;
            }
        }
    }

    #[cfg(target_endian = "little")]
    pub fn write_table<I: InterruptManager>(
        &mut self,
        offset: u64,
        data: &[u8],
        intr_mgr: &mut DeviceInterruptManager<I>,
    ) -> std::result::Result<(), std::io::Error> {
        let index: usize = (offset / MSIX_TABLE_ENTRIES_MODULO) as usize;
        let modulo_offset = (offset % MSIX_TABLE_ENTRIES_MODULO) as usize;

        assert!(data.len() <= 8 && modulo_offset + data.len() <= 0x10);
        if modulo_offset + data.len() <= MSIX_TABLE_ENTRIES_MODULO as usize {
            let config = &mut self.table_entries[index];
            let old_masked = config.masked();
            let buf = &mut config.as_mut_slice()[modulo_offset..modulo_offset + data.len()];

            buf.copy_from_slice(data);

            if self.enabled {
                // Vector configuration may have been changed.
                if modulo_offset < 0xc {
                    intr_mgr.set_msi_high_address(index as InterruptIndex, config.msg_addr_hi)?;
                    intr_mgr.set_msi_low_address(index as InterruptIndex, config.msg_addr_lo)?;
                    intr_mgr.set_msi_data(index as InterruptIndex, config.msg_data)?;
                    intr_mgr.update(index as InterruptIndex)?;
                }

                // Vector mask flag may have been changed.
                if modulo_offset + data.len() >= 0xc {
                    // The device global mask takes precedence over per vector mask.
                    if !self.masked {
                        let group = intr_mgr.get_group().unwrap();
                        if !old_masked && config.masked() {
                            group.mask(index as InterruptIndex)?;
                        } else if old_masked && !config.masked() {
                            group.unmask(index as InterruptIndex)?;
                        }
                    }
                }
            }
        } else {
            debug!("invalid data length");
        }

        Ok(())
    }

    pub fn read_pba<I: InterruptManager>(
        &mut self,
        offset: u64,
        data: &mut [u8],
        intr_mgr: &mut DeviceInterruptManager<I>,
    ) {
        assert!(data.len() <= 8);

        for ptr in data.iter_mut() {
            *ptr = 0;
        }

        if self.enabled {
            // Safe to unwrap because it's in enabled state.
            let group = intr_mgr.get_group().unwrap();
            let start = offset as InterruptIndex * 8;
            let end = min(start + data.len() as InterruptIndex * 8, group.len());

            for idx in start..end {
                if self.table_entries[idx as usize].masked() && group.get_pending_state(idx) {
                    data[(idx / 8 - offset as InterruptIndex) as usize] |= 0x1u8 << (idx % 8);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_ioctls::{Kvm, VmFd};
    use std::sync::Arc;
    use vm_device::interrupt::KvmIrqManager;
    use vm_device::resources::{DeviceResources, MsiIrqType, Resource};

    fn create_vm_fd() -> VmFd {
        let kvm = Kvm::new().unwrap();
        kvm.create_vm().unwrap()
    }

    fn create_init_resources() -> DeviceResources {
        let mut resources = DeviceResources::new();

        resources.append(Resource::MmioAddressRange {
            base: 0xd000_0000,
            size: 0x10_0000,
        });
        resources.append(Resource::LegacyIrq(0));
        resources.append(Resource::MsiIrq {
            ty: MsiIrqType::GenericMsi,
            base: 0x200,
            size: 0x10,
        });
        resources.append(Resource::MsiIrq {
            ty: MsiIrqType::PciMsi,
            base: 0x100,
            size: 0x20,
        });
        resources.append(Resource::MsiIrq {
            ty: MsiIrqType::PciMsix,
            base: 0x300,
            size: 0x20,
        });

        resources
    }

    fn create_interrupt_manager() -> DeviceInterruptManager<Arc<KvmIrqManager>> {
        let vmfd = Arc::new(create_vm_fd());
        assert!(vmfd.create_irq_chip().is_ok());
        let intr_mgr = Arc::new(KvmIrqManager::new(vmfd.clone()));

        let resource = create_init_resources();
        assert!(intr_mgr.initialize().is_ok());
        DeviceInterruptManager::new(intr_mgr.clone(), &resource).unwrap()
    }

    #[test]
    fn test_msix_table_entry() {
        let entry = MsixTableEntry::default();

        assert_eq!(entry.msg_addr_hi, 0);
        assert_eq!(entry.msg_addr_lo, 0);
        assert_eq!(entry.msg_data, 0);
        assert_eq!(entry.vector_ctl, 0);
        assert_eq!(entry.masked(), false);
    }

    #[test]
    fn test_set_msg_ctl() {
        let mut config = MsixState::new(0x10);
        let mut intr_mgr = create_interrupt_manager();

        assert_eq!(config.enabled(), false);
        assert_eq!(config.masked(), false);

        config
            .set_msg_ctl(FUNCTION_MASK_MASK, &mut intr_mgr)
            .unwrap();
        assert_eq!(config.enabled(), false);
        assert_eq!(config.masked(), true);

        let mut buf = [0u8];
        config.read_pba(0, &mut buf, &mut intr_mgr);
        assert_eq!(buf[0], 0);
        config.write_table(0xc, &[1u8], &mut intr_mgr).unwrap();
        config.read_pba(0, &mut buf, &mut intr_mgr);
        assert_eq!(buf[0], 0);

        config.set_msg_ctl(MSIX_ENABLE_MASK, &mut intr_mgr).unwrap();
        let group = intr_mgr.get_group().unwrap();
        group.notifier(0).unwrap().write(1).unwrap();
        config.read_pba(0, &mut buf, &mut intr_mgr);
        assert_eq!(buf[0], 0x1);
        config.read_pba(0, &mut buf, &mut intr_mgr);
        assert_eq!(buf[0], 0x1);
    }

    #[test]
    fn test_read_write_table() {
        let mut intr_mgr = create_interrupt_manager();
        let mut config = MsixState::new(0x10);

        let mut buf = [0u8; 4];
        config.read_table(0x0, &mut buf);
        assert_eq!(buf, [0u8; 4]);
        config.read_table(0x4, &mut buf);
        assert_eq!(buf, [0u8; 4]);
        config.read_table(0x8, &mut buf);
        assert_eq!(buf, [0u8; 4]);
        config.read_table(0xc, &mut buf);
        assert_eq!(buf, [0u8; 4]);

        let buf2 = [0xa5u8; 4];
        config.write_table(0x4, &buf2, &mut intr_mgr).unwrap();
        config.read_table(0x4, &mut buf);
        assert_eq!(buf, buf2);

        let buf3 = [0x1u8; 4];
        config.write_table(0xc, &buf3, &mut intr_mgr).unwrap();
        config.read_table(0xc, &mut buf);
        config.set_msg_ctl(MSIX_ENABLE_MASK, &mut intr_mgr).unwrap();
        assert_eq!(config.table_entries[0].masked(), true);
    }

    #[test]
    fn test_msix_cap_structure() {
        let mut msix = MsixCap::new(0x1, 0x100, 0x1000, 0x1, 0x10_0000);

        assert_eq!(msix.masked(), false);
        assert_eq!(msix.enabled(), false);
        assert_eq!(msix.table_size(), 0x100);
        assert_eq!(msix.table_bir(), 0x1);
        assert_eq!(msix.table_offset(), 0x1000);
        assert_eq!(msix.pba_offset(), 0x10_0000);
        assert_eq!(msix.pba_bir(), 0x1);

        msix.set_msg_ctl(FUNCTION_MASK_MASK | MSIX_ENABLE_MASK | 0x3ff);
        assert_eq!(msix.masked(), true);
        assert_eq!(msix.enabled(), true);
        assert_eq!(msix.table_size(), 0x100);

        assert_eq!(msix.cap_next, 0);
        assert_eq!(msix.cap_id, PciCapabilityID::MSIX as u8);
        let msg_ctl = msix.msg_ctl;
        assert_eq!(msix.read_u16(0x2), msg_ctl);
        msix.write_u16(0x2, MSIX_ENABLE_MASK);
        assert_eq!(msix.enabled(), true);
        assert_eq!(msix.masked(), false);
    }
}
