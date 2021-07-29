/*
 * Copyright (C) 2021 The Aero Project Developers.
 *
 * This file is part of The Aero Project.
 *
 * Aero is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * Aero is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with Aero. If not, see <https://www.gnu.org/licenses/>.
 */

use alloc::sync::Arc;

use alloc::vec::Vec;
use bit_field::BitField;
use spin::mutex::SpinMutex;
use spin::Once;

use crate::arch::interrupts;
use crate::mem::paging::*;
use crate::utils::{IrqGuard, VolatileCell};

use super::pci::*;

static DRIVER: Once<Arc<AhciDriver>> = Once::new();

bitflags::bitflags! {
    struct HbaEnclosureCtrl: u32 {
        const STS_MR =      1 << 0;  // Message Received
        const CTL_TM =      1 << 8;  // Transmit Message
        const CTL_RST =     1 << 9;  // Reset
        const SUPP_LED =    1 << 16; // LED Message Types
        const SUPP_SAFTE =  1 << 17; // SAF-TE Enclosure Management Messages
        const SUPP_SES2 =   1 << 18; // SES-2 Enclosure Management Messages
        const SUPP_SGPIO =  1 << 19; // SGPIO Enclosure Management Messages
        const ATTR_SMB =    1 << 24; // Single Message Buffer
        const ATTR_XMT =    1 << 25; // Transmit Only
        const ATTR_ALHD =   1 << 26; // Activity LED Hardware Driven
        const ATTR_PM =     1 << 27; // Port Multiplier Support
    }
}

bitflags::bitflags! {
    struct HbaCapabilities2: u32 {
        const BOH   = 1 << 0; // BIOS/OS Handoff
        const NVMP  = 1 << 1; // NVMHCI Present
        const APST  = 1 << 2; // Automatic Partial to Slumber Transitions
        const SDS   = 1 << 3; // Supports Device Sleep
        const SADM  = 1 << 4; // Supports Aggressive Device Sleep management
        const DESO  = 1 << 5; // DevSleep Entrance from Slumber Only
    }
}

bitflags::bitflags! {
    struct HbaBohc: u32 {
        const BOS =     1 << 0; // BIOS Owned Semaphore
        const OOS =     1 << 1; // OS Owned Semaphore
        const SOOE =    1 << 2; // SMI on OS Ownership Change Enable
        const OOC =     1 << 3; // OS Ownership Change
        const BB =      1 << 4; // BIOS Busy
    }
}

bitflags::bitflags! {
    struct HbaCapabilities: u32 {
        const SXS           = 1 << 5;  // Supports External SATA
        const EMS           = 1 << 6;  // Enclosure Management Supported
        const CCCS          = 1 << 7;  // Command Completion Coalescing Supported
        const PSC           = 1 << 13; // Partial State Capable
        const SSC           = 1 << 14; // Slumber State Capable
        const PMD           = 1 << 15; // PIO Multiple DRQ Block
        const FBSS          = 1 << 16; // FIS-based Switching Supported
        const SPM           = 1 << 17; // Supports Port Multiplier
        const SAM           = 1 << 18; // Supports AHCI mode only
        const SCLO          = 1 << 24; // Supports Command List Override
        const SAL           = 1 << 25; // Supports Activity LED
        const SALP          = 1 << 26; // Supports Aggressive Link Power Mgmt
        const SSS           = 1 << 27; // Supports Staggered Spin-up
        const SMPS          = 1 << 28; // Supports Mechanical Presence Switch
        const SSNTF         = 1 << 29; // Supports SNotification Register
        const SNCQ          = 1 << 30; // Supports Native Command Queuing
        const S64A          = 1 << 31; // Supports 64-bit Addressing
    }
}

bitflags::bitflags! {
    struct HbaHostCont: u32 {
        const HR =   1 << 0;  // HBA Reset
        const IE =   1 << 1;  // Interrupt Enable
        const MRSM = 1 << 2;  // MSI Revert to Single Message
        const AE =   1 << 31; // AHCI Enable
    }
}

bitflags::bitflags! {
    struct HbaPortIS: u32 {
        const DHRS = 1 << 0; // Device to Host Register FIS Interrupt
        const PSS = 1 << 1; // PIO Setup FIS Interrupt
        const DSS = 1 << 2; // DMA Setup FIS Interrupt
        const SDBS = 1 << 3; // Set Device Bits Interrupt
        const UFS = 1 << 4; // Unknown FIS Interrupt
        const DPS = 1 << 5; // Descriptor Processed
        const PCS = 1 << 6; // Port Connect Change Status
        const DMPS = 1 << 7; // Device Mechanical Presence Status
        const PRCS = 1 << 22; // PhyRdy Change Status
        const IPMS = 1 << 23; // Incorrect Port Multiplier Status
        const OFS = 1 << 24; // Overflow Status
        const INFS = 1 << 26; // Interface Not-fatal Error Status
        const IFS = 1 << 27; // Interface Fatal Error Status
        const HBDS = 1 << 28; // Host Bus Data Error Status
        const HBFS = 1 << 29; // Host Bus Fatal Error Status
        const TFES = 1 << 30; // Task File Error Status
        const CPDS = 1 << 31; // Cold Port Detect Status
    }
}

bitflags::bitflags! {
    struct HbaPortIE: u32 {
        const DHRE = 1 << 0; // Device to Host Register FIS Interrupt
        const PSE = 1 << 1; // PIO Setup FIS Interrupt
        const DSE = 1 << 2; // DMA Setup FIS Interrupt
        const SDBE = 1 << 3; // Set Device Bits Interrupt
        const UFE = 1 << 4; // Unknown FIS Interrupt
        const DPE = 1 << 5; // Descriptor Processed
        const PCE = 1 << 6; // Port Connect Change Status
        const DMPE = 1 << 7; // Device Mechanical Presence Status
        const PRCE = 1 << 22; // PhyRdy Change Status
        const IPME = 1 << 23; // Incorrect Port Multiplier Status
        const OFE= 1 << 24; // Overflow Status
        const INFE = 1 << 26; // Interface Not-fatal Error Status
        const IFE = 1 << 27; // Interface Fatal Error Status
        const HBDE = 1 << 28; // Host Bus Data Error Status
        const HBFE = 1 << 29; // Host Bus Fatal Error Status
        const TFEE = 1 << 30; // Task File Error Status
        const CPDE = 1 << 31; // Cold Port Detect Status
    }
}

bitflags::bitflags! {
    struct HbaPortCmd: u32 {
        const ST = 1 << 0; // Start
        const SUD = 1 << 1; // Spin-Up Device
        const POD = 1 << 2; // Power On Device
        const CLO = 1 << 3; // Command List Override
        const FRE = 1 << 4; // FIS Receive Enable
        const MPSS = 1 << 13; // Mechanical Presence Switch State
        const FR = 1 << 14; // FIS Receive Running
        const CR = 1 << 15; // Command List Running
        const CPS = 1 << 16; // Cold Presence State
        const PMA = 1 << 17; // Port Multiplier Attached
        const HPCP = 1 << 18; // Hot Plug Capable Port
        const MSPC = 1 << 19; // Mechanical Presence Switch Attached to Port
        const CPD = 1 << 20; // Cold Presence Detection
        const ESP = 1 << 21; // External SATA Port
        const FBSCP = 1 << 22; // FIS-based Switching Capable Port
        const APSTE = 1 << 23; // Automatic Partial to Slumber Transition Enabled
        const ATAPI = 1 << 24; // Device is ATAPI
        const DLAE = 1 << 25; // Drive LED on ATAPI Enable
        const ALPE = 1 << 26; // Aggressive Link Power Management Enable
        const ASP = 1 << 27; // Aggressive Slumber / Partial
    }
}

bitflags::bitflags! {
    pub struct HbaCmdHeaderFlags: u16 {
        const A = 1 << 5; // ATAPI
        const W = 1 << 6; // Write
        const P = 1 << 7; // Prefetchable
        const R = 1 << 8; // Reset
        const B = 1 << 9; // Bist
        const C = 1 << 10; // Clear Busy upon R_OK
    }
}

enum DmaCommand {
    Read,
}

struct DmaBuffer {
    /// The start address of the DMA buffer.
    start: PhysAddr,
    /// The data size of the DMA buffer.
    data_size: usize,

    /// True if the sector size is greator then 4KiB and we need
    /// to allocate a huge page for it.
    huge: bool,
}

struct DmaRequest {
    sector: usize,
    count: usize,
    buffer: Vec<DmaBuffer>,
    command: DmaCommand,
}

impl DmaRequest {
    /// Creates a new DMA request for the given sector and count.
    pub fn new(sector: usize, count: usize) -> Self {
        let mut size = count * 512;
        let mut buffer = Vec::<DmaBuffer>::new();

        while size > 0 {
            let huge = size > 0x1000; // Check if we want to allocate a huge page?
            let data_size = core::cmp::min(size, 0x2000);

            let frame = unsafe { FRAME_ALLOCATOR.allocate_frame() }
                .expect("Failed to allocate frame for DMA request");

            buffer.push(DmaBuffer {
                start: frame.start_address(),
                data_size,

                huge,
            });

            size -= data_size; // Subtract the data size from the total size.
        }

        Self {
            sector,
            count,
            buffer,
            command: DmaCommand::Read,
        }
    }

    /// Copys the data from the DMA buffer into the given buffer.
    fn copy_into(&self, into: &mut [u8]) {
        let mut offset = 0x00; // Keep track of the offset
        let mut remaning = into.len(); // Keep track of the remaining data

        for buffer in self.buffer.iter() {
            let count = core::cmp::min(remaning, 0x2000);

            let buffer_address = unsafe { crate::PHYSICAL_MEMORY_OFFSET + buffer.start.as_u64() };
            let buffer_pointer = buffer_address.as_ptr();
            let buffer = unsafe { core::slice::from_raw_parts::<u8>(buffer_pointer, count) };

            // Copy the data from the buffer into the given buffer with the
            // calculated offset.
            into[offset..offset + count].copy_from_slice(buffer);

            remaning -= count; // Subtract the size from the remaining size.
            offset += count; // Add the size to the offset.
        }
    }

    fn into_command(&self) -> AtaCommand {
        let lba48 = self.sector > 0x0FFF_FFFF;

        match self.command {
            DmaCommand::Read => {
                if lba48 {
                    AtaCommand::AtaCommandReadDmaExt
                } else {
                    AtaCommand::AtaCommandReadDma
                }
            }
        }
    }

    fn at_offset(&self, offset: usize) -> &[DmaBuffer] {
        &self.buffer[offset / 16..]
    }
}

#[allow(unused)]
#[derive(PartialEq)]
#[repr(u8)]
enum AtaCommand {
    AtaCommandWriteDma = 0xCA,
    AtaCommandWriteDmaQueued = 0xCC,
    AtaCommandWriteMultiple = 0xC5,
    AtaCommandWriteSectors = 0x30,

    AtaCommandReadDma = 0xC8,
    AtaCommandReadDmaQueued = 0xC7,
    AtaCommandReadMultiple = 0xC4,
    AtaCommandReadSectors = 0x20,

    AtaCommandWriteDmaExt = 0x35,
    AtaCommandWriteDmaQueuedExt = 0x36,
    AtaCommandWriteMultipleExt = 0x39,
    AtaCommandWriteSectorsExt = 0x34,

    AtaCommandReadDmaExt = 0x25,
    AtaCommandReadDmaQueuedExt = 0x26,
    AtaCommandReadMultipleExt = 0x29,
    AtaCommandReadSectorsExt = 0x24,

    AtaCommandPacket = 0xA0,
    AtaCommandDeviceReset = 0x08,

    AtaCommandService = 0xA2,
    AtaCommandNop = 0,
    AtaCommandNopNopAutopoll = 1,

    AtaCommandGetMediaStatus = 0xDA,

    AtaCommandFlushCache = 0xE7,
    AtaCommandFlushCacheExt = 0xEA,

    AtaCommandDataSetManagement = 0x06,

    AtaCommandMediaEject = 0xED,

    AtaCommandIdentifyPacketDevice = 0xA1,
    AtaCommandIdentifyDevice = 0xEC,

    AtaCommandSetFeatures = 0xEF,
    AtaCommandSetFeaturesEnableReleaseInt = 0x5D,
    AtaCommandSetFeaturesEnableServiceInt = 0x5E,
    AtaCommandSetFeaturesDisableReleaseInt = 0xDD,
    AtaCommandSetFeaturesDisableServiceInt = 0xDE,
}

#[repr(C)]
pub struct HbaMemory {
    host_capability: VolatileCell<HbaCapabilities>,
    global_host_control: VolatileCell<HbaHostCont>,
    interrupt_status: VolatileCell<u32>,
    ports_implemented: VolatileCell<u32>,
    version: VolatileCell<u32>,
    ccc_control: VolatileCell<u32>,
    ccc_ports: VolatileCell<u32>,
    enclosure_management_location: VolatileCell<u32>,
    enclosure_management_control: VolatileCell<HbaEnclosureCtrl>,
    host_capabilities_extended: VolatileCell<HbaCapabilities2>,
    bios_handoff_ctrl_sts: VolatileCell<HbaBohc>,
    _reserved: [u8; 0xa0 - 0x2c],
    vendor: [u8; 0x100 - 0xa0],
}

#[repr(C)]
pub struct FisRegH2D {
    fis_type: VolatileCell<FisType>,
    flags: VolatileCell<u8>,
    command: VolatileCell<AtaCommand>,
    featurel: VolatileCell<u8>,

    lba0: VolatileCell<u8>,
    lba1: VolatileCell<u8>,
    lba2: VolatileCell<u8>,
    device: VolatileCell<u8>,

    lba3: VolatileCell<u8>,
    lba4: VolatileCell<u8>,
    lba5: VolatileCell<u8>,
    featureh: VolatileCell<u8>,

    count: VolatileCell<u16>,
    icc: VolatileCell<u8>,
    control: VolatileCell<u8>,

    _reserved: [u8; 4],
}

#[repr(C)]
pub struct HbaCmdTbl {
    cfis: [u8; 64],
    acmd: [u8; 16],
    _reserved: [u8; 48],

    prdt_entry: [HbaPrdtEntry; 1],
}

impl HbaCmdTbl {
    fn cfis_as_h2d_mut(&mut self) -> &mut FisRegH2D {
        unsafe { &mut *(self.cfis.as_mut_ptr() as *mut FisRegH2D) }
    }

    fn prdt_entry_mut(&mut self, i: usize) -> &mut HbaPrdtEntry {
        unsafe { &mut *self.prdt_entry.as_mut_ptr().offset(i as isize) }
    }
}

#[repr(C)]
pub struct HbaPrdtEntry {
    dba: VolatileCell<PhysAddr>,
    _reserved: u32,
    flags: VolatileCell<u32>,
}

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum FisType {
    RegH2D = 0x27,
}

enum HbaPortDd {
    None = 0,
    PresentNotE = 1,
    PresentAndE = 3,
    Offline = 4,
}

enum HbaPortIpm {
    None = 0,
    Active = 1,
    Partial = 2,
    Slumber = 6,
    DevSleep = 8,
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct HbaSataStatus(u64);

impl HbaSataStatus {
    fn device_detection(&self) -> HbaPortDd {
        match self.0.get_bits(0..=3) {
            0 => HbaPortDd::None,
            1 => HbaPortDd::PresentNotE,
            3 => HbaPortDd::PresentAndE,
            4 => HbaPortDd::Offline,
            v => panic!("Invalid HbaPortSstsRegDet {}", v),
        }
    }

    fn interface_power_management(&self) -> HbaPortIpm {
        match self.0.get_bits(8..=11) {
            0 => HbaPortIpm::None,
            1 => HbaPortIpm::Active,
            2 => HbaPortIpm::Partial,
            6 => HbaPortIpm::Slumber,
            8 => HbaPortIpm::DevSleep,
            v => panic!("Invalid HbaPortSstsRegIpm {}", v),
        }
    }
}

#[repr(C)]
struct HbaPort {
    clb: VolatileCell<PhysAddr>,
    fb: VolatileCell<PhysAddr>,
    is: VolatileCell<HbaPortIS>,
    ie: VolatileCell<HbaPortIE>,
    cmd: VolatileCell<HbaPortCmd>,
    _reserved: u32,
    tfd: VolatileCell<u32>,
    sig: VolatileCell<u32>,
    ssts: VolatileCell<HbaSataStatus>,
    sctl: VolatileCell<u32>,
    serr: VolatileCell<u32>,
    sact: VolatileCell<u32>,
    ci: VolatileCell<u32>,
    sntf: VolatileCell<u32>,
    fbs: VolatileCell<u32>,
    devslp: VolatileCell<u32>,
    _reserved_1: [u32; 10],
    vendor: [u32; 4],
}

#[repr(C)]
struct HbaCmdHeader {
    flags: VolatileCell<HbaCmdHeaderFlags>,
    prdtl: VolatileCell<u16>,
    prdbc: VolatileCell<u32>,
    ctb: VolatileCell<PhysAddr>,
    _reserved: [u32; 4],
}

impl HbaPort {
    fn cmd_header_at(&mut self, index: usize) -> &mut HbaCmdHeader {
        // Since the CLB holds the physical address, we make the address mapped
        // before reading it.
        let clb_mapped = unsafe { crate::PHYSICAL_MEMORY_OFFSET + self.clb.get().as_u64() };
        // Get the address of the command header at `index`.
        let clb_addr = clb_mapped + core::mem::size_of::<HbaCmdHeader>() * index;

        // Cast it as [`HbaCmdHeader`] and return a mutable reference to it.
        unsafe { &mut *(clb_addr).as_mut_ptr::<HbaCmdHeader>() }
    }

    /// This function is responsible for allocating space for command lists,
    /// tables, etc.. for a given this instance of HBA port.
    fn start(&mut self) {
        self.stop_cmd(); // Stop the command engine before starting the port

        // Allocate area for for the command list.
        let frame = unsafe { FRAME_ALLOCATOR.allocate_frame() }
            .expect("Failed to allocate space for the command list");

        self.clb.set(frame.start_address());

        // Allocate area for FISs.
        let frame = unsafe { FRAME_ALLOCATOR.allocate_frame() }
            .expect("Failed to allocate space for the FISs");

        // Set the address that received FISes will be copied to.
        self.fb.set(frame.start_address());

        for i in 0..32 {
            let frame = unsafe { FRAME_ALLOCATOR.allocate_frame() }
                .expect("Here is a nickel kid, go and buy your self a real computer");

            let command_header = self.cmd_header_at(i);

            // 8 prdt entries per command table
            // 256 bytes per command table, 64 + 16 + 48 + 16 * 8
            command_header.prdtl.set(8);
            command_header.prdbc.set(0);
            command_header.ctb.set(frame.start_address());
        }

        self.start_cmd(); // Start the command engine...
    }

    fn start_cmd(&mut self) {
        while self.cmd.get().contains(HbaPortCmd::CR) {
            interrupts::pause();
        }

        let value = self.cmd.get() | (HbaPortCmd::FRE | HbaPortCmd::ST);
        self.cmd.set(value);
    }

    fn stop_cmd(&mut self) {
        let mut cmd = self.cmd.get();
        cmd.remove(HbaPortCmd::FRE | HbaPortCmd::ST);

        self.cmd.set(cmd);

        while self.cmd.get().intersects(HbaPortCmd::FR | HbaPortCmd::CR) {
            interrupts::pause();
        }
    }

    fn probe(&mut self, port: usize) -> bool {
        let status = self.ssts.get();

        let ipm = status.interface_power_management();
        let dd = status.device_detection();

        // Check if the port is active and is present. If thats the case
        // we can start the AHCI port.
        if let (HbaPortDd::PresentAndE, HbaPortIpm::Active) = (dd, ipm) {
            log::trace!("Enabling AHCI port {}", port);

            self.start();
            true
        } else {
            // Else we can't enable the port.
            false
        }
    }

    fn run_command(
        &mut self,
        command: AtaCommand,
        sector: usize,
        count: usize,
        slot: usize,
        buffer: &[DmaBuffer],
    ) {
        let header = self.cmd_header_at(slot);
        let mut flags = header.flags.get();

        if command == AtaCommand::AtaCommandWriteDmaExt || command == AtaCommand::AtaCommandWriteDma
        {
            flags.insert(HbaCmdHeaderFlags::W); // If its a write command add the write flag.
        } else {
            flags.remove(HbaCmdHeaderFlags::W); // If its a read command remove the write flag.
        }

        flags.insert(HbaCmdHeaderFlags::P | HbaCmdHeaderFlags::C);
        flags
            .bits
            .set_bits(0..=4, (core::mem::size_of::<FisRegH2D>() / 4) as u16);

        header.flags.set(flags); // Update command header flags.

        let length = ((count - 1) >> 4) + 1;
        header.prdtl.set(length as _); // Update the number of PRD entries.

        let command_table_addr =
            unsafe { crate::PHYSICAL_MEMORY_OFFSET + header.ctb.get().as_u64() };

        let command_table = unsafe { &mut *(command_table_addr).as_mut_ptr::<HbaCmdTbl>() };

        for pri in 0..length {
            let prdt = command_table.prdt_entry_mut(pri);

            prdt.dba.set(buffer[pri].start);
            prdt.flags.set(
                *prdt
                    .flags
                    .get()
                    .set_bits(0..=21, (buffer[pri].data_size - 1) as _),
            );

            // TODO: Set interrupt on completion...
        }

        let fis = command_table.cfis_as_h2d_mut();

        fis.fis_type.set(FisType::RegH2D);
        fis.flags.set(0x00);
        fis.featurel.set(0x00);
        fis.featureh.set(0x00);
        fis.lba0.set(sector as u8);
        fis.lba1.set((sector >> 8) as u8);
        fis.lba2.set((sector >> 16) as u8);
        fis.lba3.set((sector >> 24) as u8);
        fis.lba4.set((sector >> 32) as u8);
        fis.lba5.set((sector >> 40) as u8);
        fis.device.set(1 << 6);
        fis.count.set(count as _);
        fis.icc.set(0x00);
        fis.control.set(0x00);

        fis.flags.set(*fis.flags.get().set_bit(7, true));

        // Issue the command!
        self.ci.set(1 << slot);

        // Wait for the command to complete.
        loop {
            if self.ci.get() & (1 << slot) == 0 {
                break;
            }
        }
    }
}

impl HbaMemory {
    fn port_mut(&mut self, port: usize) -> &mut HbaPort {
        unsafe { &mut *((self as *mut Self).offset(1) as *mut HbaPort).offset(port as isize) }
    }
}

struct AhciCommand {
    request: Arc<DmaRequest>,
}

struct AhciPortProtected {
    address: VirtAddr,
    cmds: [Option<AhciCommand>; 32],
    free_cmds: usize,
}

impl AhciPortProtected {
    fn hba_port(&mut self) -> &mut HbaPort {
        unsafe { &mut *(self.address.as_mut_ptr::<HbaPort>()) }
    }

    fn run_request(&mut self, request: Arc<DmaRequest>, mut offset: usize) -> usize {
        let mut remaining = request.count - offset;

        while remaining > 0 {
            let slot = {
                let command =
                    self.cmds
                        .iter()
                        .enumerate()
                        .find_map(|(i, e)| if e.is_none() { Some(i) } else { None });

                if let Some(i) = command {
                    let hba = self.hba_port();
                    let count = core::cmp::min(remaining, 128);

                    hba.run_command(
                        request.into_command(),
                        request.sector + offset,
                        count,
                        i,
                        request.at_offset(offset),
                    );

                    remaining -= count;
                    offset += count;

                    i
                } else {
                    return offset;
                }
            };

            self.cmds[slot] = Some(AhciCommand {
                request: request.clone(),
            });

            self.free_cmds -= 1;
        }

        offset
    }
}

struct AhciPort {
    inner: SpinMutex<AhciPortProtected>,
}

impl AhciPort {
    #[inline]
    fn new(address: VirtAddr) -> Self {
        const EMPTY: Option<AhciCommand> = None;

        Self {
            inner: SpinMutex::new(AhciPortProtected {
                address,
                cmds: [EMPTY; 32],
                free_cmds: 32,
            }),
        }
    }

    fn run_request(&self, request: Arc<DmaRequest>) -> Option<usize> {
        let mut offset = 0x00;

        // Run request and wait for it to complete.
        while offset < request.count {
            let _guard = IrqGuard::new(); // We do not want to be interrupted while running the request.

            offset = self.inner.lock().run_request(request.clone(), offset);
        }

        Some(request.count * 512)
    }

    fn read(&self, sector: usize, buffer: &mut [u8]) -> Option<usize> {
        let count = (buffer.len() + 512 - 1) / 512;
        let request = Arc::new(DmaRequest::new(sector, count));

        let result = self.run_request(request.clone()); // Perform the DMA request.

        if result.is_some() {
            request.copy_into(buffer); // Copy the result into the provided buffer.
        }

        result
    }
}

struct AhciProtected {
    ports: [Option<Arc<AhciPort>>; 32],
    hba: VirtAddr,
}

impl AhciProtected {
    #[inline]
    fn hba_mem(&self) -> &mut HbaMemory {
        unsafe { &mut *(self.hba.as_u64() as *mut HbaMemory) }
    }

    fn start_hba(&mut self) {
        let mut hba = self.hba_mem();
        let current_flags = hba.global_host_control.get();

        hba.global_host_control.set(current_flags | HbaHostCont::IE); // Enable Interrupts

        let pi = hba.ports_implemented.get();

        for i in 0..32 {
            if pi.get_bit(i) {
                let port = hba.port_mut(i);

                if port.probe(i) {
                    // Get the address of the HBA port.
                    let address = VirtAddr::new(port as *const _ as _);

                    drop(port); // Drop the reference to the port.
                    drop(hba); // Drop the reference to the HBA.

                    let port = Arc::new(AhciPort::new(address));

                    // Add the port to the ports array.
                    self.ports[i] = Some(port);

                    // Workaround to get access to the HBA and still satify the
                    // borrow checker.
                    hba = self.hba_mem();
                }
            }
        }
    }

    /// This function is responsible for enabling bus mastering and add AHCI
    /// IRQ handler.
    fn enable_interrupts(&mut self, header: &PciHeader) {
        header.enable_bus_mastering();
    }

    /// This function is responsible for initializing and starting the AHCI driver.
    fn start_driver(&mut self, header: &PciHeader) -> Result<(), MapToError<Size4KiB>> {
        let abar = unsafe { header.get_bar(5).expect("Failed to get ABAR") };

        let (abar_address, _) = match abar {
            Bar::Memory32 { address, size, .. } => (address as u64, size as u64),
            Bar::Memory64 { address, size, .. } => (address, size),
            Bar::IO { .. } => panic!("ABAR is in port space o_O"),
        };

        self.hba = unsafe { crate::PHYSICAL_MEMORY_OFFSET + abar_address }; // Update the HBA address.

        self.start_hba();
        self.enable_interrupts(header);

        Ok(())
    }
}

/// Structure representing the ACHI driver.
struct AhciDriver {
    inner: SpinMutex<AhciProtected>,
}

impl PciDeviceHandle for AhciDriver {
    fn handles(&self, vendor_id: Vendor, device_id: DeviceType) -> bool {
        match (vendor_id, device_id) {
            (Vendor::Intel, DeviceType::SataController) => true,

            _ => false,
        }
    }

    fn start(&self, header: &PciHeader, _offset_table: &mut OffsetPageTable) {
        log::info!("Starting AHCI driver...");

        // Disable interrupts as we do not want to be interrupted durning
        // the initialization of the AHCI driver.
        let lock = IrqGuard::new();

        get_ahci().inner.lock().start_driver(header).unwrap(); // Start and initialize the AHCI controller.

        // Now the AHCI driver is initialized, we drop the IRQ lock.
        core::mem::drop(lock);

        // Temporary testing...
        if let Some(port) = get_ahci().inner.lock().ports[0].clone() {
            let buffer = &mut [0u8; 512];
            port.read(0, buffer);
            log::info!("Read sector 0: {:?}", buffer);
        }
    }
}

/// Returns a reference-counting pointer to the AHCI driver.
fn get_ahci() -> &'static Arc<AhciDriver> {
    DRIVER
        .get()
        .expect("Attempted to get the AHCI driver before it was initialized")
}

/// This function is responsible for initializing and running the AHCI driver.
pub fn ahci_init() {
    // Initialize the AHCI driver instance.
    DRIVER.call_once(|| {
        const EMPTY: Option<Arc<AhciPort>> = None; // To satisfy the Copy trait bound when the AHCI creating data.

        Arc::new(AhciDriver {
            inner: SpinMutex::new(AhciProtected {
                ports: [EMPTY; 32],    // Initialize the AHCI ports to an empty slice.
                hba: VirtAddr::zero(), // Initialize the AHCI HBA address to zero.
            }),
        })
    });

    // Now register the AHCI driver with the PCI subsystem.
    register_device_driver(get_ahci().clone());
}

crate::module_init!(ahci_init);
