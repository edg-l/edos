pub mod descriptors;
pub mod regs;

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{Ordering, compiler_fence};
use x86_64::{
    VirtAddr, structures::paging::PageTableFlags, structures::paging::mapper::MapToError,
};

use crate::{
    drivers::{
        dma::{DmaBuffer, dma},
        msi,
        pci::{
            config::{pci_read_u16, pci_write_u16, read_bar_phys},
            pci_manager,
        },
    },
    interrupts::InterruptIndex,
    log,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    net::device::NetDevice,
    thread::{runqueue::IO_PRIORITY, util::queue_spawn_kthread_named},
};

use self::{
    descriptors::{
        RX_STATUS_DD, RxDescriptor, TX_CMD_EOP, TX_CMD_IFCS, TX_CMD_RS, TX_STATUS_DD, TxDescriptor,
    },
    regs::*,
};
use crate::thread::scheduler::{current_thread, thread_park};

pub const NUM_RX_DESC: usize = 256;
pub const NUM_TX_DESC: usize = 256;
pub const RX_BUFFER_SIZE: usize = 4096;

pub struct E1000e {
    mmio_base: VirtAddr,
    rx_ring: DmaBuffer,
    tx_ring: DmaBuffer,
    rx_buffers: Box<[Option<DmaBuffer>; NUM_RX_DESC]>,
    tx_buffers: Box<[Option<DmaBuffer>; NUM_TX_DESC]>,
    rx_tail: usize,
    tx_tail: usize,
    tx_clean: usize,
    pub mac_addr: [u8; 6],
}

impl E1000e {
    fn read_reg(&self, offset: u32) -> u32 {
        unsafe { core::ptr::read_volatile((self.mmio_base + offset as u64).as_ptr::<u32>()) }
    }

    fn write_reg(&self, offset: u32, val: u32) {
        unsafe {
            core::ptr::write_volatile((self.mmio_base + offset as u64).as_mut_ptr::<u32>(), val)
        }
    }

    pub fn new(
        pci_device: &crate::drivers::pci::structures::PciDevice,
    ) -> Result<Self, alloc::string::String> {
        use alloc::format;

        // Enable PCI bus mastering
        let mut cmd = pci_read_u16(pci_device.address, 0x04);
        cmd |= 0x04;
        pci_write_u16(pci_device.address, 0x04, cmd);

        // Read BAR0
        let bar0_phys = read_bar_phys(pci_device.address, 0);

        // Map MMIO (128KB)
        let mmio_virt = get_virt_addr_from_phys_offset(bar0_phys);
        {
            let mut mapper = memory_mapper();
            match mapper.map_address_range(
                mmio_virt,
                bar0_phys,
                0x20000,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::GLOBAL,
            ) {
                Ok(_) => {}
                Err(MapToError::PageAlreadyMapped(_)) => {}
                Err(e) => return Err(format!("MMIO map failed: {:?}", e)),
            }
        }

        let mut nic = Self {
            mmio_base: mmio_virt,
            rx_ring: dma()
                .allocate_sized(core::mem::size_of::<[RxDescriptor; NUM_RX_DESC]>())
                .map_err(|e| format!("rx ring alloc: {:?}", e))?,
            tx_ring: dma()
                .allocate_sized(core::mem::size_of::<[TxDescriptor; NUM_TX_DESC]>())
                .map_err(|e| format!("tx ring alloc: {:?}", e))?,
            rx_buffers: Box::new(core::array::from_fn(|_| None)),
            tx_buffers: Box::new(core::array::from_fn(|_| None)),
            rx_tail: 0,
            tx_tail: 0,
            tx_clean: 0,
            mac_addr: [0u8; 6],
        };

        // Software reset
        let ctrl = nic.read_reg(CTRL);
        nic.write_reg(CTRL, ctrl | CTRL_RST);

        // Spin until RST clears (up to ~1s at ~1MHz polls)
        let mut reset_ok = false;
        for _ in 0..1_000_000 {
            core::hint::spin_loop();
            if nic.read_reg(CTRL) & CTRL_RST == 0 {
                reset_ok = true;
                break;
            }
        }
        if !reset_ok {
            return Err("e1000e: reset timed out".into());
        }

        // Read MAC from RAL0/RAH0
        let ral = nic.read_reg(RAL0);
        let rah = nic.read_reg(RAH0);
        let ral_bytes = ral.to_le_bytes();
        nic.mac_addr[0] = ral_bytes[0];
        nic.mac_addr[1] = ral_bytes[1];
        nic.mac_addr[2] = ral_bytes[2];
        nic.mac_addr[3] = ral_bytes[3];
        nic.mac_addr[4] = (rah & 0xFF) as u8;
        nic.mac_addr[5] = ((rah >> 8) & 0xFF) as u8;

        // Disable all interrupts
        nic.write_reg(IMC, 0xFFFFFFFF);

        // Set interrupt throttle rate (~10us coalescing, 0x28 * 256ns)
        nic.write_reg(ITR, 0x0028);

        // Zero-fill descriptor rings
        unsafe {
            core::ptr::write_bytes(nic.rx_ring.as_ptr(), 0, nic.rx_ring.size);
            core::ptr::write_bytes(nic.tx_ring.as_ptr(), 0, nic.tx_ring.size);
        }

        // Allocate RX buffers and fill descriptors
        let rx_descs = nic.rx_ring.as_ptr() as *mut RxDescriptor;
        for i in 0..NUM_RX_DESC {
            let buf = dma()
                .allocate_sized(RX_BUFFER_SIZE)
                .map_err(|e| format!("rx buf alloc: {:?}", e))?;
            let phys = buf.phys_addr().as_u64();
            unsafe {
                let desc = &mut *rx_descs.add(i);
                desc.buffer_addr = phys;
                desc.status = 0;
            }
            nic.rx_buffers[i] = Some(buf);
        }

        // Program RX descriptor ring registers
        let rx_phys = nic.rx_ring.phys_addr().as_u64();
        nic.write_reg(RDBAL, (rx_phys & 0xFFFFFFFF) as u32);
        nic.write_reg(RDBAH, (rx_phys >> 32) as u32);
        nic.write_reg(
            RDLEN,
            (NUM_RX_DESC * core::mem::size_of::<RxDescriptor>()) as u32,
        );
        nic.write_reg(RDH, 0);
        nic.write_reg(RDT, (NUM_RX_DESC - 1) as u32);

        // Program TX descriptor ring registers
        let tx_phys = nic.tx_ring.phys_addr().as_u64();
        nic.write_reg(TDBAL, (tx_phys & 0xFFFFFFFF) as u32);
        nic.write_reg(TDBAH, (tx_phys >> 32) as u32);
        nic.write_reg(
            TDLEN,
            (NUM_TX_DESC * core::mem::size_of::<TxDescriptor>()) as u32,
        );
        nic.write_reg(TDH, 0);
        nic.write_reg(TDT, 0);

        // Enable RX: EN | BAM | BSIZE_4096 | SECRC
        nic.write_reg(RCTL, RCTL_EN | RCTL_BAM | RCTL_BSIZE_4096 | RCTL_SECRC);

        // Enable TX: EN | PSP | CT=0x0F | COLD=0x3F
        nic.write_reg(
            TCTL,
            TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x3F << TCTL_COLD_SHIFT),
        );

        // Set link up
        let ctrl = nic.read_reg(CTRL);
        nic.write_reg(CTRL, ctrl | CTRL_SLU);

        // Clear pending interrupts
        let _ = nic.read_reg(ICR);

        // Configure IVAR: map RXQ0, TXQ0, and OTHER all to MSI-X vector 0.
        // Each field: [2:0]=vector number, [3]=valid bit.
        let ivar_entry = 0 | IVAR_VALID; // vector 0, valid
        nic.write_reg(IVAR, ivar_entry | (ivar_entry << 8) | (ivar_entry << 16));

        // Set EIAC to auto-clear ICR bits on MSI-X delivery. Without this,
        // ICR bits accumulate (read doesn't clear with MSI-X) and the NIC
        // stops firing new interrupts because raised_causes = IMS & ICR & ~old
        // computes to 0 when old already has all bits set.
        nic.write_reg(EIAC, ICR_RXQ0 | ICR_TXQ0 | ICR_OTHER);

        // Set CTRL_EXT.IAME to prevent IMS from being auto-cleared alongside
        // ICR when EIAC triggers.
        let ctrl_ext = nic.read_reg(CTRL_EXT);
        nic.write_reg(CTRL_EXT, ctrl_ext | CTRL_EXT_IAME);

        // Enable MSI-X (not plain MSI - QEMU e1000e routes all interrupts
        // through MSI-X when both are available, ignoring plain MSI entirely).
        msi::enable_msix_for_device(pci_device, InterruptIndex::E1000e.as_u8(), 0)
            .map_err(|e| format!("MSI-X setup failed: {:?}", e))?;

        Ok(nic)
    }

    pub fn enable_interrupts(&self) {
        // Enable both legacy cause bits (for ICR tracking) and MSI-X queue
        // cause bits (which actually trigger MSI-X delivery via IVAR).
        self.write_reg(
            IMS,
            IMS_TXDW | IMS_LSC | IMS_RXT0 | ICR_RXQ0 | ICR_TXQ0 | ICR_OTHER,
        );
    }

    pub fn handle_interrupt(&self) -> u32 {
        let icr = self.read_reg(ICR);
        // With MSI-X, reading ICR does NOT auto-clear. EIAC handles the
        // auto-clear on MSI-X delivery for the causes we configured, but
        // we also write-clear here for any bits EIAC didn't cover.
        if icr != 0 {
            self.write_reg(ICR, icr);
        }
        icr
    }

    pub fn receive(&mut self) -> Option<(DmaBuffer, usize)> {
        let rx_descs = self.rx_ring.as_ptr() as *mut RxDescriptor;
        let desc = unsafe { &mut *rx_descs.add(self.rx_tail) };

        if desc.status & RX_STATUS_DD == 0 {
            return None;
        }

        let length = desc.length as usize;

        // Allocate replacement BEFORE taking the old buffer to avoid
        // corrupting ring state if allocation fails.
        let new_buffer = dma().allocate_sized(RX_BUFFER_SIZE).ok()?;
        let old_buffer = self.rx_buffers[self.rx_tail].take()?;

        desc.buffer_addr = new_buffer.phys_addr().as_u64();
        desc.status = 0;
        desc.length = 0;

        self.rx_buffers[self.rx_tail] = Some(new_buffer);
        let old_tail = self.rx_tail;
        self.rx_tail = (old_tail + 1) % NUM_RX_DESC;
        // Ensure descriptor writes are visible before HW reads them via DMA.
        compiler_fence(Ordering::Release);
        // Write old_tail (the refilled slot) to give it back to HW.
        // The HW owns [RDH, RDT), so advancing RDT past old_tail expands the range.
        self.write_reg(RDT, old_tail as u32);

        Some((old_buffer, length))
    }

    pub fn reclaim_tx_buffers(&mut self) {
        let tx_descs = self.tx_ring.as_ptr() as *mut TxDescriptor;
        while self.tx_clean != self.tx_tail {
            let desc = unsafe { &*tx_descs.add(self.tx_clean) };
            if desc.status & TX_STATUS_DD == 0 {
                break;
            }
            if let Some(buf) = self.tx_buffers[self.tx_clean].take() {
                let _ = dma().dealloc(buf);
            }
            self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
        }
    }

    pub fn transmit(&mut self, data: &[u8]) -> Result<(), &'static str> {
        self.reclaim_tx_buffers();

        if data.len() > 1514 {
            return Err("packet too large");
        }

        let next = (self.tx_tail + 1) % NUM_TX_DESC;
        if next == self.tx_clean {
            return Err("tx ring full");
        }

        let buf = dma().allocate_sized(4096).map_err(|_| "dma alloc failed")?;
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), buf.as_ptr(), data.len());
        }

        let tx_descs = self.tx_ring.as_ptr() as *mut TxDescriptor;
        let desc = unsafe { &mut *tx_descs.add(self.tx_tail) };
        desc.buffer_addr = buf.phys_addr().as_u64();
        desc.length = data.len() as u16;
        desc.cmd = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS;
        desc.status = 0;

        self.tx_buffers[self.tx_tail] = Some(buf);
        self.tx_tail = next;
        // Ensure descriptor writes are visible before HW reads them via DMA.
        compiler_fence(Ordering::Release);
        self.write_reg(TDT, self.tx_tail as u32);

        Ok(())
    }
}

impl NetDevice for E1000e {
    fn send(&mut self, data: &[u8]) -> Result<(), &'static str> {
        self.transmit(data)
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac_addr
    }

    fn link_up(&self) -> bool {
        self.read_reg(STATUS) & STATUS_LU != 0
    }
}

pub extern "C" fn e1000e_driver_main() -> ! {
    use crate::{
        interrupts::io::E1000E_DRIVER_THREAD_ID,
        net::stack::{NET_STACK, NetStack},
    };

    let thread = current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);
    E1000E_DRIVER_THREAD_ID.call_once(|| Arc::downgrade(&thread));

    let devices = pci_manager().read().get_devices().to_vec();
    let pci_dev = devices
        .iter()
        .find(|d| d.header.vendor_id == 0x8086 && d.header.device_id == 0x10D3)
        .copied();

    let Some(pci_dev) = pci_dev else {
        log!("e1000e: no device found");
        loop {
            thread_park();
        }
    };

    let mut nic = match E1000e::new(&pci_dev) {
        Ok(nic) => nic,
        Err(e) => {
            log!("e1000e: init failed: {}", e);
            loop {
                thread_park();
            }
        }
    };

    log!(
        "e1000e: initialized, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        nic.mac_addr[0],
        nic.mac_addr[1],
        nic.mac_addr[2],
        nic.mac_addr[3],
        nic.mac_addr[4],
        nic.mac_addr[5]
    );

    // Run DHCP to acquire IP configuration before publishing the stack.
    let mac = nic.mac_addr;
    let lease = crate::net::dhcp::discover(&mut nic, mac);

    // Move the NIC into the NetStack and publish the global.
    NET_STACK.call_once(|| {
        let mut stack = NetStack::new(nic);
        if let Some(ref lease) = lease {
            stack.local_ip = lease.ip;
            stack.subnet_mask = lease.subnet_mask;
            stack.gateway_ip = lease.gateway;
        } else {
            log!("e1000e: DHCP failed, using fallback 10.0.2.15");
            stack.local_ip = [10, 0, 2, 15];
            stack.subnet_mask = [255, 255, 255, 0];
            stack.gateway_ip = [10, 0, 2, 2];
        }
        spin::Mutex::new(stack)
    });

    {
        let s = crate::net::stack::net_stack().lock();
        log!(
            "e1000e: network ready, IP {}.{}.{}.{}",
            s.local_ip[0],
            s.local_ip[1],
            s.local_ip[2],
            s.local_ip[3]
        );
    }

    // Spawn the TCP retransmit timer thread.
    queue_spawn_kthread_named(
        "tcp-retransmit",
        crate::net::stack::tcp_retransmit_main as *const () as u64,
    );

    // Enable interrupts now that the stack is published.
    {
        let stack = crate::net::stack::net_stack().lock();
        let _ = stack.nic.handle_interrupt(); // clear any stale ICR from DHCP
        stack.nic.enable_interrupts();
    }

    loop {
        // Park until an MSI-X interrupt wakes us (RX, TX, or LSC).
        thread_park();

        {
            let stack = crate::net::stack::net_stack().lock();
            let icr = stack.nic.handle_interrupt();

            if icr & IMS_LSC != 0 {
                let status = stack.nic.read_reg(STATUS);
                log!("e1000e: link status change, up={}", status & STATUS_LU != 0);
            }
        }

        // Receive all pending packets, copy data out, then dispatch.
        // Socket poll notifications are accumulated in pending_socket_notifs
        // and flushed after dropping the NetStack lock.
        loop {
            let (packet, notifs) = {
                let mut stack = crate::net::stack::net_stack().lock();
                let pkt = match stack.nic.receive() {
                    Some((buf, len)) => {
                        let data =
                            unsafe { core::slice::from_raw_parts(buf.as_ptr(), len) }.to_vec();
                        let _ = dma().dealloc(buf);
                        Some(data)
                    }
                    None => None,
                };
                // Drain any notifications from previous handle_rx calls in this burst
                let n = core::mem::take(&mut stack.pending_socket_notifs);
                (pkt, n)
            };

            // Flush notifications outside the lock
            for n in notifs {
                n.flush();
            }

            match packet {
                Some(data) => {
                    let notifs = {
                        let mut stack = crate::net::stack::net_stack().lock();
                        stack.handle_rx(&data);
                        core::mem::take(&mut stack.pending_socket_notifs)
                    };
                    for n in notifs {
                        n.flush();
                    }
                }
                None => break,
            }
        }

        crate::net::stack::net_stack()
            .lock()
            .nic
            .reclaim_tx_buffers();
    }
}

pub fn init() {
    queue_spawn_kthread_named("e1000e", e1000e_driver_main as *const () as u64);
}
