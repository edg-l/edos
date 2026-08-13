# EDOS GUI Implementation Plan

This document outlines the implementation plan for adding a graphical user interface to EDOS with mouse support, windows, focus management, and a user-space compositor architecture.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        User Space                                │
│                                                                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────────────┐  │
│  │  App 1   │  │  App 2   │  │  App 3   │  │   Compositor   │  │
│  │          │  │          │  │          │  │   (edos-wm)    │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └───────┬────────┘  │
│       │             │             │                 │           │
│       │         libedos_gui       │                 │           │
│       └─────────────┴─────────────┘                 │           │
│                     │                               │           │
│              Shared Memory Buffers                  │           │
│                     │                               │           │
└─────────────────────┼───────────────────────────────┼───────────┘
                      │                               │
┌─────────────────────┼───────────────────────────────┼───────────┐
│                     │         Kernel                │           │
│                     ▼                               ▼           │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │                    Window Server                         │   │
│  │  - Window registry (id, pid, rect, z-order)             │   │
│  │  - Input event routing                                   │   │
│  │  - Shared memory management                              │   │
│  └─────────────────────────────────────────────────────────┘   │
│                              │                                  │
│              ┌───────────────┼───────────────┐                 │
│              ▼               ▼               ▼                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐         │
│  │   Keyboard   │  │    Mouse     │  │  Framebuffer │         │
│  │   Driver     │  │    Driver    │  │    Driver    │         │
│  └──────────────┘  └──────────────┘  └──────────────┘         │
└─────────────────────────────────────────────────────────────────┘
```

## Implementation Phases

---

## Phase 1: Mouse Driver

**Goal:** Add PS/2 mouse support with event broadcasting.

### 1.0 Existing Infrastructure

The kernel already has partial mouse support:

**In `kernel/src/interrupts/mod.rs`:**
```rust
Mouse = APIC_OFFSET + 2,  // InterruptIndex::Mouse defined
```

**In `kernel/src/apic/init.rs` (lines 111-143):**
- IRQ12 is detected from ACPI tables
- IO APIC entry configured for mouse interrupt
- IRQ enabled

**In `kernel/src/interrupts/io.rs`:**
```rust
pub(super) extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { get_lapic().end_of_interrupt() };
    // Currently does nothing - just sends EOI
}
```

### 1.1 What Needs to be Added

**Files to create/modify:**
- `kernel/src/drivers/mouse/mod.rs` (NEW)
- `kernel/src/drivers/mod.rs` (add mouse module)
- `kernel/src/interrupts/io.rs` (call into mouse driver)

**Implementation details:**

### 1.1 PS/2 Mouse Protocol

The PS/2 mouse shares the controller with keyboard (ports 0x60 data, 0x64 command).

```rust
// kernel/src/drivers/mouse/mod.rs

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use crossbeam_queue::ArrayQueue;
use x86_64::instructions::port::Port;

use crate::{
    fs::{DevFsDevice, DevFsError, PollState, register_device_str},
    thread::{broadcast::Broadcaster, mutex::BlockingMutex},
};

// PS/2 ports
const DATA_PORT: u16 = 0x60;
const CMD_PORT: u16 = 0x64;

// Commands to controller (write to 0x64)
const CMD_ENABLE_AUX: u8 = 0xA8;      // Enable auxiliary (mouse) port
const CMD_GET_CONFIG: u8 = 0x20;      // Read config byte
const CMD_SET_CONFIG: u8 = 0x60;      // Write config byte
const CMD_WRITE_AUX: u8 = 0xD4;       // Send next byte to mouse

// Commands to mouse (write to 0x60 after CMD_WRITE_AUX)
const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_STREAMING: u8 = 0xF4;
const MOUSE_SET_SAMPLE_RATE: u8 = 0xF3;
const MOUSE_GET_DEVICE_ID: u8 = 0xF2;

// Mouse packet bits
const PACKET_LEFT_BTN: u8 = 0x01;
const PACKET_RIGHT_BTN: u8 = 0x02;
const PACKET_MIDDLE_BTN: u8 = 0x04;
const PACKET_X_SIGN: u8 = 0x10;
const PACKET_Y_SIGN: u8 = 0x20;
const PACKET_X_OVERFLOW: u8 = 0x40;
const PACKET_Y_OVERFLOW: u8 = 0x80;

#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub x: i32,           // Absolute X position
    pub y: i32,           // Absolute Y position
    pub dx: i16,          // Relative X movement
    pub dy: i16,          // Relative Y movement
    pub buttons: u8,      // Button state (bit 0=left, 1=right, 2=middle)
    pub scroll: i8,       // Scroll wheel delta (if supported)
}

pub static MOUSE_BROADCAST: Broadcaster<MouseEvent> = Broadcaster::new();
static MOUSE_POSITION: (AtomicI32, AtomicI32) = (AtomicI32::new(0), AtomicI32::new(0));
static MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);
static PACKET_QUEUE: BlockingMutex<ArrayQueue<u8>> =
    BlockingMutex::new(ArrayQueue::new(256));

// Screen bounds (set during init)
static SCREEN_WIDTH: AtomicI32 = AtomicI32::new(800);
static SCREEN_HEIGHT: AtomicI32 = AtomicI32::new(600);

pub fn set_screen_bounds(width: i32, height: i32) {
    SCREEN_WIDTH.store(width, Ordering::Relaxed);
    SCREEN_HEIGHT.store(height, Ordering::Relaxed);
}

pub fn get_position() -> (i32, i32) {
    (
        MOUSE_POSITION.0.load(Ordering::Relaxed),
        MOUSE_POSITION.1.load(Ordering::Relaxed),
    )
}
```

### 1.2 Initialization Sequence

```rust
pub fn init() {
    // 1. Enable auxiliary port
    wait_write();
    unsafe { Port::new(CMD_PORT).write(CMD_ENABLE_AUX) };

    // 2. Enable interrupts for aux port
    wait_write();
    unsafe { Port::new(CMD_PORT).write(CMD_GET_CONFIG) };
    wait_read();
    let mut config: u8 = unsafe { Port::new(DATA_PORT).read() };
    config |= 0x02;  // Enable IRQ12 (aux port interrupt)
    config &= !0x20; // Enable aux port clock

    wait_write();
    unsafe { Port::new(CMD_PORT).write(CMD_SET_CONFIG) };
    wait_write();
    unsafe { Port::new(DATA_PORT).write(config) };

    // 3. Reset mouse to defaults
    mouse_write(MOUSE_SET_DEFAULTS);
    mouse_read(); // ACK

    // 4. Try to enable scroll wheel (magic sequence)
    // Set sample rate: 200, 100, 80 to enable wheel
    for rate in [200u8, 100, 80] {
        mouse_write(MOUSE_SET_SAMPLE_RATE);
        mouse_read(); // ACK
        mouse_write(rate);
        mouse_read(); // ACK
    }

    // Check device ID (3 = wheel mouse, 4 = 5-button)
    mouse_write(MOUSE_GET_DEVICE_ID);
    mouse_read(); // ACK
    let device_id = mouse_read();
    let has_wheel = device_id >= 3;

    // 5. Enable streaming mode
    mouse_write(MOUSE_ENABLE_STREAMING);
    mouse_read(); // ACK

    // 6. Register device and start driver thread
    let device = Arc::new(MouseDevice { has_wheel });
    register_device_str("/mouse", device).expect("failed to register mouse");

    queue_spawn_kthread_named("mouse", driver_main as *const () as u64);
}

fn wait_write() {
    while unsafe { Port::<u8>::new(CMD_PORT).read() } & 0x02 != 0 {}
}

fn wait_read() {
    while unsafe { Port::<u8>::new(CMD_PORT).read() } & 0x01 == 0 {}
}

fn mouse_write(byte: u8) {
    wait_write();
    unsafe { Port::new(CMD_PORT).write(CMD_WRITE_AUX) };
    wait_write();
    unsafe { Port::new(DATA_PORT).write(byte) };
}

fn mouse_read() -> u8 {
    wait_read();
    unsafe { Port::new(DATA_PORT).read() }
}
```

### 1.3 Interrupt Handler & Packet Processing

```rust
// Called from IRQ12 handler
pub fn handle_interrupt() {
    let byte: u8 = unsafe { Port::new(DATA_PORT).read() };
    if let Some(queue) = PACKET_QUEUE.try_lock() {
        let _ = queue.push(byte);
    }
}

// Driver thread
extern "C" fn driver_main() {
    let packet_size = 3; // or 4 if has_wheel
    let mut packet = [0u8; 4];
    let mut packet_idx = 0;

    loop {
        // Drain queue
        let queue = PACKET_QUEUE.lock();
        while let Some(byte) = queue.pop() {
            // Sync: first byte must have bit 3 set
            if packet_idx == 0 && (byte & 0x08) == 0 {
                continue; // Resync
            }

            packet[packet_idx] = byte;
            packet_idx += 1;

            if packet_idx >= packet_size {
                process_packet(&packet[..packet_size]);
                packet_idx = 0;
            }
        }
        drop(queue);

        sched().thread_sleep(Duration::from_millis(1));
    }
}

fn process_packet(packet: &[u8]) {
    let buttons = packet[0] & 0x07;

    // Extract signed deltas
    let mut dx = packet[1] as i16;
    let mut dy = packet[2] as i16;

    if packet[0] & PACKET_X_SIGN != 0 {
        dx |= 0xFF00u16 as i16; // Sign extend
    }
    if packet[0] & PACKET_Y_SIGN != 0 {
        dy |= 0xFF00u16 as i16;
    }

    // PS/2 mouse Y is inverted
    dy = -dy;

    // Ignore overflow
    if packet[0] & (PACKET_X_OVERFLOW | PACKET_Y_OVERFLOW) != 0 {
        return;
    }

    // Update absolute position with clamping
    let max_x = SCREEN_WIDTH.load(Ordering::Relaxed);
    let max_y = SCREEN_HEIGHT.load(Ordering::Relaxed);

    let old_x = MOUSE_POSITION.0.load(Ordering::Relaxed);
    let old_y = MOUSE_POSITION.1.load(Ordering::Relaxed);

    let new_x = (old_x + dx as i32).clamp(0, max_x - 1);
    let new_y = (old_y + dy as i32).clamp(0, max_y - 1);

    MOUSE_POSITION.0.store(new_x, Ordering::Relaxed);
    MOUSE_POSITION.1.store(new_y, Ordering::Relaxed);
    MOUSE_BUTTONS.store(buttons, Ordering::Relaxed);

    // Scroll wheel (4th byte if present)
    let scroll = if packet.len() >= 4 {
        packet[3] as i8
    } else {
        0
    };

    let event = MouseEvent {
        x: new_x,
        y: new_y,
        dx,
        dy,
        buttons,
        scroll,
    };

    MOUSE_BROADCAST.broadcast(event);
}
```

### 1.4 DevFS Interface

```rust
struct MouseDevice {
    has_wheel: bool,
}

impl DevFsDevice for MouseDevice {
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        // Return current state as MouseEvent bytes
        let (x, y) = get_position();
        let buttons = MOUSE_BUTTONS.load(Ordering::Relaxed);

        let event = MouseEvent {
            x, y,
            dx: 0, dy: 0,
            buttons,
            scroll: 0,
        };

        let bytes = unsafe {
            core::slice::from_raw_parts(
                &event as *const MouseEvent as *const u8,
                core::mem::size_of::<MouseEvent>()
            )
        };

        Ok(bytes[..count.min(bytes.len())].to_vec())
    }

    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(MousePoll))
    }

    // ... other methods
}
```

### 1.5 IRQ12 Handler Registration

Add to `kernel/src/interrupts/mod.rs`:

```rust
// In IDT setup
idt[44].set_handler_fn(irq12_handler); // IRQ12 = interrupt 44

extern "x86-interrupt" fn irq12_handler(_frame: InterruptStackFrame) {
    crate::drivers::mouse::handle_interrupt();

    // Send EOI to both PICs (IRQ12 is on slave)
    unsafe {
        Port::<u8>::new(0xA0).write(0x20); // Slave EOI
        Port::<u8>::new(0x20).write(0x20); // Master EOI
    }
}
```

---

## Phase 2: Shared Memory Extensions

**Goal:** Extend existing mmap to support shared memory between processes.

### 2.1 Existing Implementation

The kernel already has mmap in `kernel/src/syscalls/memory.rs` with:
- `sys_mmap(addr, length, prot, flags)` - supports `MAP_ANONYMOUS | MAP_PRIVATE`
- `sys_munmap(addr, length)` - basic unmapping
- Protection flags: `PROT_READ`, `PROT_WRITE`, `PROT_EXEC`
- Automatic virtual address allocation

### 2.2 What Needs to be Added

**Files to modify/create:**
- `kernel/src/syscalls/memory.rs` (extend)
- `kernel/src/memory/shared.rs` (new)
- `kernel/src/syscalls/shm.rs` (new)

#### 2.2.1 Add MAP_SHARED Support

The current mmap only supports private mappings. For GUI, we need shared mappings so the compositor can access client window buffers.

```rust
// Add to kernel/src/syscalls/memory.rs

const MAP_SHARED: u32 = 0x01;  // Add this flag

// In sys_mmap, handle MAP_SHARED:
if (flags & MAP_SHARED) != 0 {
    // Create SharedMemory object that tracks physical frames
    // Store in global registry with unique ID
    // Return mapping that references the shared object
}
```

#### 2.2.2 SharedMemory Tracking

```rust
// kernel/src/memory/shared.rs

use alloc::{sync::Arc, vec::Vec};
use x86_64::PhysAddr;

/// A shared memory region that can be mapped into multiple address spaces
pub struct SharedMemory {
    /// Physical frames backing this region
    frames: Vec<PhysAddr>,
    /// Size in bytes
    size: usize,
    /// Unique identifier
    id: u64,
    /// Reference count (number of mappings)
    ref_count: AtomicUsize,
}

impl SharedMemory {
    pub fn new(size: usize) -> Result<Arc<Self>, Error> {
        let page_count = (size + 4095) / 4096;
        let mut frames = Vec::with_capacity(page_count);

        for _ in 0..page_count {
            let frame = allocate_frame()?;
            frames.push(frame);
        }

        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        Ok(Arc::new(Self {
            frames,
            size,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            ref_count: AtomicUsize::new(1),
        }))
    }

    /// Map this shared memory into an address space
    pub fn map_into(&self, mapper: &mut MemoryManager, virt_base: VirtAddr) -> Result<(), Error> {
        for (i, &phys) in self.frames.iter().enumerate() {
            let virt = virt_base + (i * 4096) as u64;
            mapper.map_frame(virt, phys, PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE)?;
        }
        self.ref_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Global registry of shared memory objects
pub static SHARED_MEMORY_REGISTRY: RwLock<BTreeMap<u64, Arc<SharedMemory>>> = RwLock::new(BTreeMap::new());
```

#### 2.2.3 Shared Memory Handle Passing

For the compositor to map a client's buffer, we need syscalls to share memory by ID:

```rust
// kernel/src/syscalls/shm.rs

/// Create anonymous shared memory, returns shm_id
pub fn sys_shm_create(size: usize) -> i64 {
    let shared = SharedMemory::new(size)?;
    let id = shared.id;
    SHARED_MEMORY_REGISTRY.write().insert(id, shared);
    id as i64
}

/// Map shared memory by ID into calling process
pub fn sys_shm_map(shm_id: u64, addr_hint: u64) -> u64 {
    let registry = SHARED_MEMORY_REGISTRY.read();
    let shared = registry.get(&shm_id)?;

    // Find virtual address and map
    let virt_base = find_free_virtual_address(...);
    shared.map_into(mapper, virt_base)?;

    virt_base.as_u64()
}

/// Unmap shared memory from calling process
pub fn sys_shm_unmap(addr: u64) -> i64 {
    // Unmap and decrement ref count
}

/// Destroy shared memory (only if ref_count == 0)
pub fn sys_shm_destroy(shm_id: u64) -> i64 {
    // Remove from registry, free physical frames if no refs
}
```

---

## Phase 3: Window Server (Kernel Component)

**Goal:** Kernel-side window registry and input routing.

**Files to create:**
- `kernel/src/window/mod.rs`
- `kernel/src/window/registry.rs`
- `kernel/src/window/input.rs`

### 3.1 Window Registry

```rust
// kernel/src/window/registry.rs

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use spin::RwLock;

pub type WindowId = u64;

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: WindowId,
    pub pid: u64,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub z_order: u32,
    pub visible: bool,
    pub title: String,
    /// Shared memory ID for pixel buffer
    pub buffer_shm_id: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width as i32 &&
        y >= self.y && y < self.y + self.height as i32
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width as i32 &&
        self.x + self.width as i32 > other.x &&
        self.y < other.y + other.height as i32 &&
        self.y + self.height as i32 > other.y
    }
}

pub static WINDOW_REGISTRY: RwLock<WindowRegistry> = RwLock::new(WindowRegistry::new());

pub struct WindowRegistry {
    windows: BTreeMap<WindowId, WindowInfo>,
    next_id: WindowId,
    z_order_counter: u32,
    focused_window: Option<WindowId>,
}

impl WindowRegistry {
    pub const fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            next_id: 1,
            z_order_counter: 0,
            focused_window: None,
        }
    }

    pub fn create_window(&mut self, pid: u64, x: i32, y: i32, width: u32, height: u32) -> WindowId {
        let id = self.next_id;
        self.next_id += 1;
        self.z_order_counter += 1;

        let window = WindowInfo {
            id,
            pid,
            x, y, width, height,
            z_order: self.z_order_counter,
            visible: false,
            title: String::new(),
            buffer_shm_id: None,
        };

        self.windows.insert(id, window);
        id
    }

    pub fn destroy_window(&mut self, id: WindowId) -> Option<WindowInfo> {
        if self.focused_window == Some(id) {
            self.focused_window = None;
        }
        self.windows.remove(&id)
    }

    pub fn get_window(&self, id: WindowId) -> Option<&WindowInfo> {
        self.windows.get(&id)
    }

    pub fn get_window_mut(&mut self, id: WindowId) -> Option<&mut WindowInfo> {
        self.windows.get_mut(&id)
    }

    pub fn set_focused(&mut self, id: WindowId) {
        if self.windows.contains_key(&id) {
            self.focused_window = Some(id);
            // Bring to top
            self.z_order_counter += 1;
            if let Some(w) = self.windows.get_mut(&id) {
                w.z_order = self.z_order_counter;
            }
        }
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        self.focused_window
    }

    /// Get window at screen coordinates (top-most)
    pub fn window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        self.windows
            .values()
            .filter(|w| w.visible)
            .filter(|w| {
                let rect = Rect { x: w.x, y: w.y, width: w.width, height: w.height };
                rect.contains(x, y)
            })
            .max_by_key(|w| w.z_order)
            .map(|w| w.id)
    }

    /// Get all visible windows sorted by z-order (back to front)
    pub fn visible_windows_sorted(&self) -> Vec<&WindowInfo> {
        let mut windows: Vec<_> = self.windows.values().filter(|w| w.visible).collect();
        windows.sort_by_key(|w| w.z_order);
        windows
    }
}
```

### 3.2 Input Routing

```rust
// kernel/src/window/input.rs

use super::registry::{WINDOW_REGISTRY, WindowId};
use crate::drivers::{keyboard::KEYBOARD_BROADCAST, mouse::MOUSE_BROADCAST};

#[derive(Debug, Clone)]
pub enum WindowEvent {
    // Mouse events (coordinates relative to window)
    MouseMove { x: i32, y: i32 },
    MouseButton { x: i32, y: i32, button: u8, pressed: bool },
    MouseScroll { x: i32, y: i32, delta: i8 },

    // Keyboard events
    KeyPress { key: u32 },
    KeyRelease { key: u32 },
    Character { ch: char },

    // Window events
    FocusGained,
    FocusLost,
    CloseRequested,
    Resize { width: u32, height: u32 },

    // Compositor events
    Expose { x: i32, y: i32, width: u32, height: u32 },
}

/// Event queue per window
pub static WINDOW_EVENTS: RwLock<BTreeMap<WindowId, ArrayQueue<WindowEvent>>> =
    RwLock::new(BTreeMap::new());

pub fn init_input_routing() {
    // Spawn input routing thread
    queue_spawn_kthread_named("input-router", input_router_thread as *const () as u64);
}

extern "C" fn input_router_thread() {
    let mouse_rx = MOUSE_BROADCAST.subscribe();
    let kbd_rx = KEYBOARD_BROADCAST.subscribe();

    let mut last_buttons = 0u8;
    let mut last_window_under_cursor: Option<WindowId> = None;

    loop {
        // Process mouse events
        while let Ok(event) = mouse_rx.try_recv() {
            let registry = WINDOW_REGISTRY.read();
            let window_under_cursor = registry.window_at(event.x, event.y);
            drop(registry);

            // Handle focus changes on click
            if event.buttons & 0x01 != 0 && last_buttons & 0x01 == 0 {
                // Left button just pressed
                if let Some(wid) = window_under_cursor {
                    let mut registry = WINDOW_REGISTRY.write();
                    let old_focus = registry.focused_window();
                    if old_focus != Some(wid) {
                        // Send focus events
                        if let Some(old) = old_focus {
                            send_event(old, WindowEvent::FocusLost);
                        }
                        registry.set_focused(wid);
                        send_event(wid, WindowEvent::FocusGained);
                    }
                }
            }

            // Route mouse move to window under cursor
            if let Some(wid) = window_under_cursor {
                let registry = WINDOW_REGISTRY.read();
                if let Some(w) = registry.get_window(wid) {
                    let local_x = event.x - w.x;
                    let local_y = event.y - w.y;

                    send_event(wid, WindowEvent::MouseMove { x: local_x, y: local_y });

                    // Button events
                    for btn in 0..3 {
                        let mask = 1 << btn;
                        if (event.buttons & mask) != (last_buttons & mask) {
                            let pressed = (event.buttons & mask) != 0;
                            send_event(wid, WindowEvent::MouseButton {
                                x: local_x, y: local_y,
                                button: btn,
                                pressed,
                            });
                        }
                    }

                    // Scroll
                    if event.scroll != 0 {
                        send_event(wid, WindowEvent::MouseScroll {
                            x: local_x, y: local_y,
                            delta: event.scroll,
                        });
                    }
                }
            }

            last_buttons = event.buttons;
            last_window_under_cursor = window_under_cursor;
        }

        // Process keyboard events - send to focused window
        while let Ok(key) = kbd_rx.try_recv() {
            let registry = WINDOW_REGISTRY.read();
            if let Some(wid) = registry.focused_window() {
                match key {
                    DecodedKey::Unicode(ch) => {
                        send_event(wid, WindowEvent::Character { ch });
                    }
                    DecodedKey::RawKey(code) => {
                        send_event(wid, WindowEvent::KeyPress { key: code as u32 });
                    }
                }
            }
        }

        sched().thread_yield();
    }
}

fn send_event(window: WindowId, event: WindowEvent) {
    let events = WINDOW_EVENTS.read();
    if let Some(queue) = events.get(&window) {
        let _ = queue.push(event);
        // Wake up any thread polling this window
        // ... notify mechanism
    }
}
```

### 3.3 Window Syscalls

```rust
// kernel/src/syscalls/window.rs

/// Create a new window
/// Returns window ID or -1 on error
pub fn sys_window_create(x: i32, y: i32, width: u32, height: u32) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let pid = info.lock().pid;

    let mut registry = WINDOW_REGISTRY.write();
    let id = registry.create_window(pid, x, y, width, height);

    // Create event queue for this window
    WINDOW_EVENTS.write().insert(id, ArrayQueue::new(256));

    id as i64
}

/// Destroy a window
pub fn sys_window_destroy(window_id: u64) -> i64 {
    let mut registry = WINDOW_REGISTRY.write();

    // Verify ownership
    let info = sched().current_thread_info();
    let pid = info.lock().pid;

    if let Some(w) = registry.get_window(window_id) {
        if w.pid != pid {
            info.lock().errno = Errno::EPERM;
            return -1;
        }
    }

    registry.destroy_window(window_id);
    WINDOW_EVENTS.write().remove(&window_id);
    0
}

/// Set window properties
pub fn sys_window_set(window_id: u64, property: u32, value: u64) -> i64 {
    const PROP_VISIBLE: u32 = 1;
    const PROP_X: u32 = 2;
    const PROP_Y: u32 = 3;
    const PROP_WIDTH: u32 = 4;
    const PROP_HEIGHT: u32 = 5;
    const PROP_TITLE_PTR: u32 = 6;
    const PROP_BUFFER_SHM: u32 = 7;

    let mut registry = WINDOW_REGISTRY.write();

    if let Some(w) = registry.get_window_mut(window_id) {
        match property {
            PROP_VISIBLE => w.visible = value != 0,
            PROP_X => w.x = value as i32,
            PROP_Y => w.y = value as i32,
            PROP_WIDTH => w.width = value as u32,
            PROP_HEIGHT => w.height = value as u32,
            PROP_BUFFER_SHM => w.buffer_shm_id = Some(value),
            _ => return -1,
        }
        0
    } else {
        -1
    }
}

/// Poll for window events
/// Returns number of events written to buffer
pub fn sys_window_poll(window_id: u64, events_ptr: *mut WindowEvent, max_events: usize) -> i64 {
    let events = WINDOW_EVENTS.read();

    if let Some(queue) = events.get(&window_id) {
        let mut count = 0;
        while count < max_events {
            if let Some(event) = queue.pop() {
                if !unsafe { try_write_user(events_ptr.add(count), event) } {
                    return -1;
                }
                count += 1;
            } else {
                break;
            }
        }
        count as i64
    } else {
        -1
    }
}

/// Get list of all windows (for compositor)
pub fn sys_window_list(buffer_ptr: *mut WindowInfo, max_count: usize) -> i64 {
    let registry = WINDOW_REGISTRY.read();
    let windows = registry.visible_windows_sorted();

    let count = windows.len().min(max_count);
    for (i, w) in windows.iter().take(count).enumerate() {
        if !unsafe { try_write_user(buffer_ptr.add(i), (*w).clone()) } {
            return -1;
        }
    }

    count as i64
}
```

---

## Phase 4: User-Space Compositor (edos-wm)

**Goal:** User-space window manager and compositor.

**Files to create:**
- `programs/edos-wm/src/main.rs`
- `programs/edos-wm/src/compositor.rs`
- `programs/edos-wm/src/cursor.rs`
- `programs/edos-wm/src/decorations.rs`

### 4.1 Main Loop

```rust
// programs/edos-wm/src/main.rs

use elibc::graphics::{Screen, Texture, Color};

mod compositor;
mod cursor;
mod decorations;

fn main() {
    let mut screen = Screen::get();
    let screen_info = screen.info();

    // Load cursor sprite
    let cursor = cursor::load_default_cursor();

    // Damage tracking
    let mut damage_regions: Vec<Rect> = Vec::new();

    // Subscribe to input events
    let mouse_fd = open("/dev/mouse", 0);
    let kbd_fd = open("/dev/kbd", 0);

    loop {
        // 1. Poll for input events
        poll_input(mouse_fd, kbd_fd);

        // 2. Get window list from kernel
        let windows = sys_window_list();

        // 3. Composite frame
        if !damage_regions.is_empty() || cursor_moved {
            composite_frame(&mut screen, &windows, &cursor, &damage_regions);
            damage_regions.clear();
        }

        // 4. Present
        screen.present();

        // 5. Yield to other processes
        yield_thread();
    }
}
```

### 4.2 Compositor Core

```rust
// programs/edos-wm/src/compositor.rs

use elibc::graphics::{Screen, Texture, Color, BlendMode};

pub struct Compositor {
    screen: Screen,
    back_buffer: Texture,
    cursor_texture: Texture,
    cursor_x: i32,
    cursor_y: i32,
}

impl Compositor {
    pub fn new() -> Self {
        let screen = Screen::get();
        let info = screen.info();

        let back_buffer = Texture::new(info.width as u64, info.height as u64);
        let cursor_texture = load_cursor_texture();

        Self {
            screen,
            back_buffer,
            cursor_texture,
            cursor_x: (info.width / 2) as i32,
            cursor_y: (info.height / 2) as i32,
        }
    }

    pub fn composite(&mut self, windows: &[WindowInfo]) {
        // Clear to desktop background
        self.back_buffer.fill(Color::from_rgb(0x30, 0x30, 0x40));

        // Draw windows back-to-front
        for window in windows {
            self.draw_window(window);
        }

        // Draw cursor on top
        self.back_buffer.blit_with_alpha(
            &self.cursor_texture,
            self.cursor_x as u64,
            self.cursor_y as u64,
        );

        // Copy to screen
        self.screen.draw(&self.back_buffer.as_draw_request(0, 0));
    }

    fn draw_window(&mut self, window: &WindowInfo) {
        // Draw window decoration (title bar, border)
        decorations::draw_frame(
            &mut self.back_buffer,
            window.x, window.y,
            window.width, window.height,
            &window.title,
            window.focused,
        );

        // Map and draw client buffer
        if let Some(shm_id) = window.buffer_shm_id {
            // Map shared memory from client
            let client_buffer = map_client_buffer(shm_id, window.width, window.height);

            // Blit to back buffer (with clipping)
            let content_x = window.x + DECORATION_BORDER_WIDTH;
            let content_y = window.y + DECORATION_TITLE_HEIGHT;

            self.back_buffer.blit(
                &client_buffer,
                content_x as u64,
                content_y as u64,
            );
        }
    }

    pub fn set_cursor_position(&mut self, x: i32, y: i32) {
        self.cursor_x = x;
        self.cursor_y = y;
    }
}
```

### 4.3 Window Decorations

```rust
// programs/edos-wm/src/decorations.rs

use elibc::graphics::{Texture, Color, TextStyle, FontWeight, RasterHeight};

pub const TITLE_BAR_HEIGHT: u32 = 24;
pub const BORDER_WIDTH: u32 = 2;

const COLOR_TITLE_ACTIVE: Color = Color::from_rgb(0x40, 0x60, 0x90);
const COLOR_TITLE_INACTIVE: Color = Color::from_rgb(0x50, 0x50, 0x50);
const COLOR_BORDER: Color = Color::from_rgb(0x20, 0x20, 0x20);
const COLOR_TITLE_TEXT: Color = Color::WHITE;

const BUTTON_CLOSE: char = '\u{2715}';    // ×
const BUTTON_MAXIMIZE: char = '\u{25A1}'; // □
const BUTTON_MINIMIZE: char = '\u{2212}'; // −

pub fn draw_frame(
    target: &mut Texture,
    x: i32, y: i32,
    width: u32, height: u32,
    title: &str,
    focused: bool,
) {
    let total_width = width + BORDER_WIDTH * 2;
    let total_height = height + TITLE_BAR_HEIGHT + BORDER_WIDTH;

    // Border
    target.fill_rect(
        x as u64, y as u64,
        total_width as u64, total_height as u64,
        COLOR_BORDER,
    );

    // Title bar
    let title_color = if focused { COLOR_TITLE_ACTIVE } else { COLOR_TITLE_INACTIVE };
    target.fill_rect(
        (x + BORDER_WIDTH as i32) as u64,
        (y + BORDER_WIDTH as i32) as u64,
        width as u64,
        TITLE_BAR_HEIGHT as u64,
        title_color,
    );

    // Title text
    let text_style = TextStyle {
        font_weight: FontWeight::Regular,
        font_size: RasterHeight::Size16,
        foreground: COLOR_TITLE_TEXT,
        background: title_color,
    };

    target.draw_text(
        (x + BORDER_WIDTH as i32 + 8) as u64,
        (y + BORDER_WIDTH as i32 + 4) as u64,
        title,
        &text_style,
    );

    // Window buttons (close, maximize, minimize)
    let button_y = y + BORDER_WIDTH as i32 + 2;
    let button_size = 20u32;

    // Close button (red)
    let close_x = x + BORDER_WIDTH as i32 + width as i32 - button_size as i32 - 4;
    target.fill_rect(
        close_x as u64, button_y as u64,
        button_size as u64, button_size as u64,
        Color::from_rgb(0xE0, 0x40, 0x40),
    );

    // Content area background (client draws here)
    let content_x = x + BORDER_WIDTH as i32;
    let content_y = y + BORDER_WIDTH as i32 + TITLE_BAR_HEIGHT as i32;
    target.fill_rect(
        content_x as u64, content_y as u64,
        width as u64, height as u64,
        Color::WHITE, // or transparent if client has buffer
    );
}

pub struct HitTestResult {
    pub region: HitRegion,
    pub window_id: Option<WindowId>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HitRegion {
    None,
    TitleBar,
    CloseButton,
    MaximizeButton,
    MinimizeButton,
    ResizeTopLeft,
    ResizeTop,
    ResizeTopRight,
    ResizeLeft,
    ResizeRight,
    ResizeBottomLeft,
    ResizeBottom,
    ResizeBottomRight,
    Client,
}

pub fn hit_test(window: &WindowInfo, x: i32, y: i32) -> HitRegion {
    let rel_x = x - window.x;
    let rel_y = y - window.y;

    // Check if in window bounds
    let total_width = window.width + BORDER_WIDTH * 2;
    let total_height = window.height + TITLE_BAR_HEIGHT + BORDER_WIDTH;

    if rel_x < 0 || rel_y < 0 ||
       rel_x >= total_width as i32 || rel_y >= total_height as i32 {
        return HitRegion::None;
    }

    // Check resize borders (8 pixel grab zones)
    const RESIZE_BORDER: i32 = 8;

    let on_left = rel_x < RESIZE_BORDER;
    let on_right = rel_x >= total_width as i32 - RESIZE_BORDER;
    let on_top = rel_y < RESIZE_BORDER;
    let on_bottom = rel_y >= total_height as i32 - RESIZE_BORDER;

    match (on_left, on_right, on_top, on_bottom) {
        (true, _, true, _) => HitRegion::ResizeTopLeft,
        (_, true, true, _) => HitRegion::ResizeTopRight,
        (true, _, _, true) => HitRegion::ResizeBottomLeft,
        (_, true, _, true) => HitRegion::ResizeBottomRight,
        (true, _, _, _) => HitRegion::ResizeLeft,
        (_, true, _, _) => HitRegion::ResizeRight,
        (_, _, true, _) => HitRegion::ResizeTop,
        (_, _, _, true) => HitRegion::ResizeBottom,
        _ => {}
    }

    // Check title bar
    if rel_y >= BORDER_WIDTH as i32 &&
       rel_y < (BORDER_WIDTH + TITLE_BAR_HEIGHT) as i32 {
        // Check buttons
        let button_area_start = total_width as i32 - 70;
        if rel_x >= button_area_start {
            let button_idx = (rel_x - button_area_start) / 22;
            return match button_idx {
                0 => HitRegion::MinimizeButton,
                1 => HitRegion::MaximizeButton,
                2 => HitRegion::CloseButton,
                _ => HitRegion::TitleBar,
            };
        }
        return HitRegion::TitleBar;
    }

    // Must be client area
    HitRegion::Client
}
```

### 4.4 Cursor Rendering

```rust
// programs/edos-wm/src/cursor.rs

use elibc::graphics::{Texture, Color};

pub const CURSOR_WIDTH: u32 = 16;
pub const CURSOR_HEIGHT: u32 = 16;

/// Default arrow cursor bitmap (1 = white, 2 = black, 0 = transparent)
const ARROW_CURSOR: [[u8; 16]; 16] = [
    [2,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,2,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,2,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,2,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,2,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,2,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,2,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,2,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,2,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,2,2,2,2,0,0,0,0,0,0],
    [2,1,1,2,1,1,2,0,0,0,0,0,0,0,0,0],
    [2,1,2,0,2,1,1,2,0,0,0,0,0,0,0,0],
    [2,2,0,0,2,1,1,2,0,0,0,0,0,0,0,0],
    [2,0,0,0,0,2,1,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,2,1,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,2,0,0,0,0,0,0,0,0],
];

pub fn load_default_cursor() -> Texture {
    let mut texture = Texture::new(CURSOR_WIDTH as u64, CURSOR_HEIGHT as u64);
    texture.fill(Color::TRANSPARENT);

    for y in 0..16 {
        for x in 0..16 {
            let color = match ARROW_CURSOR[y][x] {
                1 => Color::WHITE,
                2 => Color::BLACK,
                _ => continue, // transparent
            };
            texture.set_pixel(x as u64, y as u64, color);
        }
    }

    texture
}

pub enum CursorShape {
    Arrow,
    Hand,
    Text,
    ResizeNS,
    ResizeEW,
    ResizeNESW,
    ResizeNWSE,
    Move,
    Wait,
}
```

---

## Phase 5: Client Library (libedos_gui)

**Goal:** Easy-to-use library for GUI applications.

**Files to create:**
- `programs/libedos_gui/src/lib.rs`
- `programs/libedos_gui/src/window.rs`
- `programs/libedos_gui/src/events.rs`
- `programs/libedos_gui/src/widgets/mod.rs`

### 5.1 Window API

```rust
// programs/libedos_gui/src/window.rs

use crate::events::Event;
use elibc::graphics::Texture;

pub struct Window {
    id: u64,
    width: u32,
    height: u32,
    buffer: Texture,
    shm_id: u64,
}

impl Window {
    /// Create a new window
    pub fn new(width: u32, height: u32, title: &str) -> Result<Self, Error> {
        // 1. Create window in kernel
        let id = unsafe { sys_window_create(0, 0, width, height) };
        if id < 0 {
            return Err(Error::CreateFailed);
        }
        let id = id as u64;

        // 2. Create shared memory for pixel buffer
        let buffer_size = (width * height * 4) as usize;
        let shm_ptr = unsafe { sys_mmap(0, buffer_size, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0) };
        if shm_ptr == !0u64 {
            unsafe { sys_window_destroy(id) };
            return Err(Error::MemoryAllocationFailed);
        }

        // 3. Create texture wrapping the shared memory
        let buffer = unsafe { Texture::from_raw_parts(shm_ptr as *mut u32, width, height) };

        // 4. Set window title
        unsafe { sys_window_set_title(id, title.as_ptr(), title.len()) };

        // 5. Register shared memory with window
        let shm_id = get_shm_id(shm_ptr);
        unsafe { sys_window_set(id, PROP_BUFFER_SHM, shm_id) };

        Ok(Self { id, width, height, buffer, shm_id })
    }

    /// Show the window
    pub fn show(&self) {
        unsafe { sys_window_set(self.id, PROP_VISIBLE, 1) };
    }

    /// Hide the window
    pub fn hide(&self) {
        unsafe { sys_window_set(self.id, PROP_VISIBLE, 0) };
    }

    /// Get the drawing surface
    pub fn canvas(&mut self) -> &mut Texture {
        &mut self.buffer
    }

    /// Signal that the window content has changed
    pub fn present(&self) {
        // Notify compositor that this window needs redraw
        unsafe { sys_window_present(self.id) };
    }

    /// Poll for events (non-blocking)
    pub fn poll_event(&self) -> Option<Event> {
        let mut event = MaybeUninit::uninit();
        let count = unsafe { sys_window_poll(self.id, event.as_mut_ptr(), 1) };
        if count > 0 {
            Some(unsafe { event.assume_init() })
        } else {
            None
        }
    }

    /// Wait for an event (blocking)
    pub fn wait_event(&self) -> Event {
        loop {
            if let Some(event) = self.poll_event() {
                return event;
            }
            // Sleep briefly
            thread_yield();
        }
    }

    /// Get window dimensions
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            sys_window_destroy(self.id);
            sys_munmap(self.buffer.as_ptr() as u64, (self.width * self.height * 4) as usize);
        }
    }
}
```

### 5.2 Event Types

```rust
// programs/libedos_gui/src/events.rs

#[derive(Debug, Clone)]
pub enum Event {
    /// Window needs to be redrawn
    Paint,

    /// Window was resized
    Resize { width: u32, height: u32 },

    /// Window close was requested
    CloseRequested,

    /// Window gained focus
    FocusIn,

    /// Window lost focus
    FocusOut,

    /// Mouse moved within window
    MouseMove { x: i32, y: i32 },

    /// Mouse button pressed
    MouseDown { x: i32, y: i32, button: MouseButton },

    /// Mouse button released
    MouseUp { x: i32, y: i32, button: MouseButton },

    /// Mouse wheel scrolled
    Scroll { x: i32, y: i32, delta: i32 },

    /// Key pressed
    KeyDown { key: Key, modifiers: Modifiers },

    /// Key released
    KeyUp { key: Key, modifiers: Modifiers },

    /// Text input (after keyboard processing)
    TextInput { character: char },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    // Letters
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z,

    // Numbers
    Num0, Num1, Num2, Num3, Num4,
    Num5, Num6, Num7, Num8, Num9,

    // Function keys
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,

    // Navigation
    Left, Right, Up, Down,
    Home, End, PageUp, PageDown,

    // Editing
    Backspace, Delete, Insert,
    Enter, Tab, Escape, Space,

    // Modifiers (as keys)
    LeftShift, RightShift,
    LeftCtrl, RightCtrl,
    LeftAlt, RightAlt,

    // Other
    CapsLock, NumLock, ScrollLock,
    PrintScreen, Pause,

    Unknown(u32),
}
```

### 5.3 Example Application

```rust
// programs/gui-demo/src/main.rs

use libedos_gui::{Window, Event, MouseButton};
use libedos_gui::graphics::{Color, TextStyle};

fn main() {
    // Create window
    let mut window = Window::new(640, 480, "Hello GUI").unwrap();

    // Initial draw
    draw(&mut window);
    window.show();
    window.present();

    // Event loop
    loop {
        match window.wait_event() {
            Event::Paint => {
                draw(&mut window);
                window.present();
            }
            Event::CloseRequested => {
                break;
            }
            Event::MouseDown { x, y, button: MouseButton::Left } => {
                println!("Click at ({}, {})", x, y);
            }
            Event::KeyDown { key, .. } => {
                println!("Key: {:?}", key);
            }
            _ => {}
        }
    }
}

fn draw(window: &mut Window) {
    let canvas = window.canvas();

    // Clear background
    canvas.fill(Color::from_rgb(0xF0, 0xF0, 0xF0));

    // Draw some text
    canvas.draw_text(20, 20, "Hello, EDOS GUI!", &TextStyle::default());

    // Draw a button
    canvas.fill_rect(20, 60, 100, 30, Color::from_rgb(0x40, 0x80, 0xF0));
    canvas.draw_text(35, 68, "Click Me", &TextStyle {
        foreground: Color::WHITE,
        ..Default::default()
    });

    // Draw a box
    canvas.stroke_rect(20, 110, 200, 100, Color::BLACK);
}
```

---

## Phase 6: Additional Features

### 6.1 Clipboard Support

Shipped, with one change from the sketch below: there are two buffers rather
than one, so a selection does not overwrite a deliberate copy, and both calls
take which buffer they mean as their first argument. See
`kernel/src/window/clipboard.rs` and Phase 5 of `doc/USERSPACE-ROADMAP.md`.

```rust
// Kernel clipboard buffer
static CLIPBOARD: RwLock<Vec<u8>> = RwLock::new(Vec::new());

pub fn sys_clipboard_get(buffer_ptr: *mut u8, max_len: usize) -> i64;
pub fn sys_clipboard_set(data_ptr: *const u8, len: usize) -> i64;
```

### 6.2 Drag and Drop

```rust
pub enum DragDropEvent {
    DragEnter { x: i32, y: i32, mime_types: Vec<String> },
    DragOver { x: i32, y: i32 },
    DragLeave,
    Drop { x: i32, y: i32, data: Vec<u8>, mime_type: String },
}
```

### 6.3 System Tray / Notifications

```rust
pub struct Notification {
    pub title: String,
    pub body: String,
    pub icon: Option<Texture>,
    pub timeout: Duration,
}

pub fn show_notification(notification: Notification);
```

### 6.4 Widget Toolkit

```rust
// Basic widgets
pub struct Button { ... }
pub struct Label { ... }
pub struct TextInput { ... }
pub struct Checkbox { ... }
pub struct Slider { ... }
pub struct ListView { ... }
pub struct ScrollArea { ... }

// Layout
pub trait Layout { ... }
pub struct VBoxLayout { ... }
pub struct HBoxLayout { ... }
pub struct GridLayout { ... }
```

---

## File Structure

```
kernel/
├── src/
│   ├── drivers/
│   │   ├── mouse/
│   │   │   └── mod.rs          # PS/2 mouse driver (NEW)
│   │   └── ...
│   ├── window/
│   │   ├── mod.rs              # Window server init (NEW)
│   │   ├── registry.rs         # Window registry (NEW)
│   │   └── input.rs            # Input routing (NEW)
│   ├── memory/
│   │   └── shared.rs           # Shared memory tracking (NEW)
│   └── syscalls/
│       ├── memory.rs           # mmap/munmap (EXISTS - extend for MAP_SHARED)
│       ├── shm.rs              # Shared memory handles (NEW)
│       └── window.rs           # Window syscalls (NEW)

programs/
├── edos-wm/                    # Compositor / Window Manager
│   └── src/
│       ├── main.rs
│       ├── compositor.rs
│       ├── cursor.rs
│       └── decorations.rs
├── libedos_gui/                # Client library
│   └── src/
│       ├── lib.rs
│       ├── window.rs
│       ├── events.rs
│       └── widgets/
│           ├── mod.rs
│           ├── button.rs
│           └── ...
└── gui-demo/                   # Demo application
    └── src/
        └── main.rs
```

---

## Implementation Order

1. **Phase 1: Mouse Driver** (~1 day)
   - PS/2 initialization
   - Interrupt handler
   - Event broadcasting
   - DevFS interface

2. **Phase 2: Shared Memory** (~2 days)
   - SharedMemory struct
   - sys_mmap / sys_munmap
   - Handle passing mechanism

3. **Phase 3: Window Server** (~3 days)
   - Window registry
   - Basic syscalls
   - Input routing thread

4. **Phase 4: Compositor** (~4 days)
   - Main loop
   - Back-to-front rendering
   - Cursor compositing
   - Window decorations

5. **Phase 5: Client Library** (~3 days)
   - Window creation API
   - Event handling
   - Drawing primitives

6. **Phase 6: Polish** (~2 days)
   - Damage tracking optimization
   - Double buffering
   - Alpha blending

**Total estimated effort: ~2-3 weeks**

---

## Testing Strategy

1. **Unit tests** for window registry, hit testing, event routing
2. **Integration test**: Create window, draw, receive events
3. **Visual test**: Run compositor with demo app
4. **Stress test**: Many windows, rapid input

---

## Performance Considerations

1. **Damage tracking**: Only redraw changed regions
2. **Double buffering**: Prevent tearing
3. **Shared memory**: Zero-copy buffer sharing
4. **Lazy compositing**: Only composite when something changes
5. **Hardware acceleration**: Future - use GPU if available

---

## Security Considerations

1. **Window ownership**: Only owning process can modify window
2. **Input routing**: Events only go to appropriate windows
3. **Shared memory**: Validate mappings, prevent unauthorized access
4. **Compositor privilege**: Only compositor can access all window buffers
