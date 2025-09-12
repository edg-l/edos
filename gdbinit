set disassemble-next-line on
set architecture i386:x86-64
set pagination off
set confirm off
set print pretty on
file kernel/target/x86_64-unknown-none/debug/edos-kernel
target remote :1234
set scheduler-locking step
hb kmain
c
