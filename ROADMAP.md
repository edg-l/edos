# x86_64 Rust Kernel Implementation Roadmap

## Phase 1: Boot & Basic Infrastructure

### 1. Serial Console
- Initialize COM1 (0x3F8) for debug output
- Implement basic write functions
- Set up early logging macros

### 2. GDT + TSS
- Set up Global Descriptor Table with kernel/user code/data segments
- Configure Task State Segment with dedicated double fault stack
- Set up IST (Interrupt Stack Table) entry 1 for double fault

### 3. IDT + Exception Handlers
- Implement Interrupt Descriptor Table
- Add critical exception handlers:
  - Page fault handler (log CR2, error code, RIP)
  - Double fault handler (using TSS IST1)
  - General protection fault
  - Invalid opcode
- Implement panic handler for Rust

## Phase 2: Memory Management

### 4. Frame Allocator
- Parse Limine memory map
- Implement physical memory allocator (bitmap or buddy system)
- Mark kernel/bootloader regions as used

### 5. Memory Mapper
- Implement virtual memory management
- Use Limine's higher-half direct mapping (HHDM)
- Page table manipulation and mapping functions
- Handle page table page allocation without heap

### 6. Heap Allocator
- Implement kernel heap using memory mapper
- Can use linked_list_allocator or custom implementation
- Set up dedicated virtual regions for heap

## Phase 3: Interrupts & Concurrency

### 7. APIC Setup
- Configure Local APIC
- Set up timer interrupts for preemption
- Configure I/O APIC for device interrupts

### 8. Context Switching
- Save/restore CPU state (registers, stack)
- Implement basic task switching mechanism
- Handle stack management

### 9. Kernel Threads
- Thread control blocks
- Basic scheduler (round-robin initially)
- Thread creation and management
- Preemptive scheduling with timer interrupts

## Phase 4: User Space Interface

### 10. Syscalls
- Set up SYSCALL/SYSRET instructions
- Implement syscall handler
- Basic syscalls (exit, write, read)
- User/kernel stack switching

## Optional Early Enhancements

### Stack Guard Pages
- Map kernel stacks with unmapped guard pages below
- Catch stack overflow early

### Improved Debug Support
- Stack unwinding for better panic messages
- Kernel symbol parsing for backtraces

### Basic Drivers
- Keyboard driver (PS/2 or USB HID)
- Simple filesystem (initrd/ramfs)

## Implementation Notes

- **Memory Layout**: Use Limine's HHDM for physical memory access
- **Virtual Addresses**: Kernel at higher half, user space in lower half
- **Error Handling**: Comprehensive page fault diagnostics essential
- **Testing**: Test double fault handler early with stack overflow
- **Dependencies**: Each phase builds on previous - don't skip ahead

## Key Rust Considerations

- `#![no_std]` environment
- Custom panic handler
- Volatile memory access for MMIO
- Careful unsafe code around page tables and context switching
- Consider using crates like `x86_64`, `spin`, `linked_list_allocator`
