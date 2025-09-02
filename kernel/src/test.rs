use core::time::Duration;

use alloc::collections::vec_deque::VecDeque;
use spin::Once;
use x86_64::instructions::hlt;

use crate::{
    serial_println,
    thread::{mailbox::Mailbox, scheduler::sched},
};

static MAILBOX1: Once<Mailbox<u64, u64>> = Once::new();

pub fn thread_1() -> ! {
    serial_println!("thread_1: initializing");
    let mailbox = MAILBOX1.call_once(|| Mailbox::new(sched().current_id()));
    serial_println!("thread_1: mailbox init done");
    let mut requests = VecDeque::new();
    let mut counter = 0;
    loop {
        // save all incoming requests
        while let Some(req) = mailbox.pop_request() {
            serial_println!("thread_1: got request");
            requests.push_back(req);
        }

        // in a more realistic scenario maybe we would use a btree and have to wait for some interrutps, here we can process directly.
        while let Some(request) = requests.pop_front() {
            serial_println!("thread_1: answering request");
            request.answer(counter);
            serial_println!("thread_1: done answering");
            counter += 1;
        }

        hlt();
    }
}

pub fn thread_2() -> ! {
    let mailbox = {
        loop {
            if let Some(value) = MAILBOX1.get() {
                break value;
            }
            hlt();
        }
    };

    let mut counter = 0;
    loop {
        serial_println!("thread_2: sending request with counter {counter}");
        let res = mailbox.send(counter);
        serial_println!("thread_2: request sent");
        counter += 1;
        serial_println!("thread_2: waiting to receive (4s timeout)...");
        let value = res.receive_timeout(Duration::from_secs(4));
        serial_println!("thread_2: received, got value: {value:?}");

        sched().thread_sleep(Duration::from_secs(1));
    }
}
