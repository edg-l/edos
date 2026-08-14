/* Process entry for a C program on EDOS.
 *
 * The kernel hands over the System V initial process stack (psABI §3.4.1):
 * `argc` at `[rsp]`, then `argv[]`, a NULL, `envp[]`, a NULL, and the auxiliary
 * vector. `%rsp` is 16-byte aligned and no return address was pushed, so this
 * cannot be an ordinary compiled function — its prologue would assume the
 * post-`call` state and the first `movaps` spill would fault. Hence the naked
 * entry below, which does what every libc's `crt1.o` does: zero `%rbp` to
 * terminate a frame-pointer walk, hand `%rsp` to a real function as its first
 * argument, and align the stack before calling it.
 *
 * The kernel also passes `argc`, `argv` and `envp` in `rdi`, `rsi` and `rdx`
 * for the benefit of programs built against its Rust runtime. Those are
 * ignored here: reading the stack is what a C runtime does, and it is the only
 * route to the auxiliary vector.
 */

extern int main(int argc, char **argv, char **envp);
extern void _exit(int status);
extern void __libc_init_array(void) __attribute__((weak));

char **environ;

__attribute__((used)) static void edos_start_c(long *stack) {
    int argc = (int)stack[0];
    char **argv = (char **)&stack[1];
    char **envp = argv + argc + 1;

    environ = envp;

    /* Static constructors, when the program has any. Weak so a build that
     * links no C++ and no __attribute__((constructor)) still resolves. */
    if (__libc_init_array) {
        __libc_init_array();
    }

    _exit(main(argc, argv, envp));
}

__asm__(".global _start\n"
        ".type _start, @function\n"
        "_start:\n"
        "  xor %ebp, %ebp\n"      /* outermost frame, per the psABI */
        "  mov %rsp, %rdi\n"      /* the whole initial stack is the argument */
        "  and $-16, %rsp\n"      /* tolerate either entry alignment */
        "  call edos_start_c\n"
        "  hlt\n"                 /* edos_start_c does not return */
        ".size _start, . - _start\n");
