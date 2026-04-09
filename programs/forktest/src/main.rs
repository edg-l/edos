use edos_lib::process;

fn main() {
    println!("forktest: starting, pid={}", process::getpid());

    let mut x = 42u64;

    let pid = process::fork();
    if pid < 0 {
        eprintln!("fork failed: {}", pid);
        return;
    }

    if pid == 0 {
        // Child
        println!("child: pid={}, x={}", process::getpid(), x);
        x = 99;
        println!("child: x changed to {}", x);

        // Test that heap works after fork
        let v: Vec<u64> = (0..10).collect();
        println!("child: vec sum = {}", v.iter().sum::<u64>());

        // Multi-fork: child forks again
        let pid2 = process::fork();
        if pid2 == 0 {
            println!("grandchild: pid={}, x={}", process::getpid(), x);
        } else if pid2 > 0 {
            println!("child: spawned grandchild pid={}", pid2);
            process::waitpid(pid2 as u64);
            println!("child: grandchild exited");
        }
    } else {
        // Parent
        println!("parent: forked child pid={}", pid);
        process::waitpid(pid as u64);
        println!("parent: child exited, x={} (should be 42)", x);
    }
}
