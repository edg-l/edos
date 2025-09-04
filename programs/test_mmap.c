// programs/test_mmap.c

static inline void* sys_mmap(void* addr, unsigned long length, int prot, int flags) {
    void* result;
    register int r10_flags asm("r10") = flags;
    __asm__ volatile (
        "mov $9, %%rax\n\t"
        "syscall\n\t"
        : "=a" (result)
        : "D" (addr), "S" (length), "d" (prot), "r" (r10_flags)
        : "r8", "r9", "rcx", "r11"
    );
    return result;
}

static inline int sys_munmap(void* addr, unsigned long length) {
    int result;
    __asm__ volatile (
        "mov $11, %%rax\n\t"
        "syscall\n\t"
        : "=a" (result)
        : "D" (addr), "S" (length)
        : "rcx", "r11"
    );
    return result;
}

static inline long sys_write(int fd, const void* buf, unsigned long count) {
    long result;
    __asm__ volatile (
        "mov $1, %%rax\n\t"
        "syscall\n\t"
        : "=a" (result)
        : "D" (fd), "S" (buf), "d" (count)
        : "rcx", "r11"
    );
    return result;
}

static inline void sys_exit(int code) {
    __asm__ volatile (
        "mov $60, %%rax\n\t"
        "syscall\n\t"
        :
        : "D" (code)
        : "rax", "rcx", "r11"
    );
    __builtin_unreachable();
}

// Protection flags
#define PROT_READ  0x1
#define PROT_WRITE 0x2
#define PROT_EXEC  0x4

// Mapping flags
#define MAP_PRIVATE    0x02
#define MAP_ANONYMOUS  0x20

// Helper to print strings
void print(const char* str) {
    const char* p = str;
    while (*p) p++; // find length
    sys_write(1, str, p - str);
}

void _start(void) {
    print("Starting mmap tests...\n");

    // Test 1: Basic mmap allocation
    print("Test 1: Basic mmap (4KB)...\n");
    void* ptr = sys_mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);

    if (ptr == (void*)-1) {
        print("FAILED: mmap returned -1\n");
        sys_exit(1);
    }

    print("SUCCESS: mmap returned valid pointer\n");

    // Test 2: Write to the mapped memory
    print("Test 2: Writing to mapped memory...\n");
    char* mem = (char*)ptr;
    mem[0] = 'H';
    mem[1] = 'e';
    mem[2] = 'l';
    mem[3] = 'l';
    mem[4] = 'o';
    mem[5] = ' ';
    mem[6] = 'f';
    mem[7] = 'r';
    mem[8] = 'o';
    mem[9] = 'm';
    mem[10] = ' ';
    mem[11] = 'm';
    mem[12] = 'm';
    mem[13] = 'a';
    mem[14] = 'p';
    mem[15] = '!';
    mem[16] = '\n';
    mem[17] = '\0';

    // Test 3: Read back and print the data
    print("Test 3: Reading back data: ");
    sys_write(1, mem, 17); // Print the message we wrote

    // Test 4: munmap
    print("Test 4: Unmapping memory...\n");
    int unmap_result = sys_munmap(ptr, 4096);
    if (unmap_result != 0) {
        print("FAILED: munmap returned non-zero\n");
        sys_exit(3);
    }
    print("SUCCESS: munmap completed\n");

    // Test 5: Try to map a larger region
    print("Test 5: Large mmap (64KB)...\n");
    void* large_ptr = sys_mmap(0, 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);
    if (large_ptr == (void*)-1) {
        print("FAILED: Large mmap returned -1\n");
        sys_exit(4);
    }

    print("SUCCESS: Large mmap allocated\n");

    // Test 6: Write to different pages
    print("Test 6: Testing demand paging across multiple pages...\n");
    char* large_mem = (char*)large_ptr;

    // Write to first page
    large_mem[0] = 'P';
    large_mem[1] = 'a';
    large_mem[2] = 'g';
    large_mem[3] = 'e';
    large_mem[4] = ' ';
    large_mem[5] = '1';
    large_mem[6] = '\n';

    // Write to second page
    large_mem[4096] = 'P';
    large_mem[4097] = 'a';
    large_mem[4098] = 'g';
    large_mem[4099] = 'e';
    large_mem[4100] = ' ';
    large_mem[4101] = '2';
    large_mem[4102] = '\n';

    // Write to last page
    large_mem[65536-7] = 'L';
    large_mem[65536-6] = 'a';
    large_mem[65536-5] = 's';
    large_mem[65536-4] = 't';
    large_mem[65536-3] = '!';
    large_mem[65536-2] = '\n';
    large_mem[65536-1] = '\0';

    // Print what we wrote to each page
    print("Page 1 content: ");
    sys_write(1, &large_mem[0], 7);

    print("Page 2 content: ");
    sys_write(1, &large_mem[4096], 7);

    print("Last page content: ");
    sys_write(1, &large_mem[65536-7], 6);

    // Clean up
    print("Test 7: Cleaning up large mapping...\n");
    sys_munmap(large_ptr, 65536);

    print("SUCCESS: All mmap tests passed!\n");
    sys_exit(42);
}
