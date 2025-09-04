// No standard library headers - freestanding

void _start(void) {
    // sys_write(1, "Hello from user space!\n", 23)
    __asm__ volatile (
        "mov $1, %%rax\n\t"     // sys_write
        "mov $1, %%rdi\n\t"     // stdout
        "mov %0, %%rsi\n\t"     // buffer
        "mov $23, %%rdx\n\t"    // count
        "syscall\n\t"

        // sys_exit(42)
        "mov $60, %%rax\n\t"
        "mov $42, %%rdi\n\t"
        "syscall\n\t"
        :
        : "r"("Hello from user space!\n")
        : "rax", "rdi", "rsi", "rdx"
    );
}

/*
gcc -nostdlib -static -no-pie -fno-stack-protector -m64 \
    -Wl,--entry=_start -o exit42 exit42.c

*/
