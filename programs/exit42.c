// No standard library headers - freestanding

void _start(void) {
    // sys_exit(42) using inline assembly
    __asm__ volatile (
        "mov $60, %%rax\n\t"    // sys_exit syscall number
        "mov $42, %%rdi\n\t"    // exit code
        "syscall\n\t"           // invoke syscall
        :
        :
        : "rax", "rdi"
    );

    // Should never reach here, but just in case
    __builtin_unreachable();
}

/*
gcc -nostdlib -static -no-pie -fno-stack-protector -m64 \
    -Wl,--entry=_start -o exit42 exit42.c

*/
