use core::time::Duration;

use alloc::collections::vec_deque::VecDeque;
use spin::Once;
use x86_64::instructions::hlt;

use crate::{
    drivers::keyboard::KEYBOARD_BROADCAST,
    graphics::{
        api::{draw_line, draw_rect, render, screen_info},
        colors::{self, hsl_to_rgb},
    },
    println,
    thread::{mailbox::Mailbox, scheduler::sched},
    timer::Instant,
};

static MAILBOX1: Once<Mailbox<u64, u64>> = Once::new();

pub fn thread_1() -> ! {
    println!("initializing");
    let mailbox = MAILBOX1.call_once(|| Mailbox::new(sched().current_id()));
    println!("mailbox init done");
    let mut requests = VecDeque::new();
    let mut counter = 0;
    loop {
        // save all incoming requests
        while let Some(req) = mailbox.pop_request() {
            println!("got request");
            requests.push_back(req);
        }

        // in a more realistic scenario maybe we would use a btree and have to wait for some interrutps, here we can process directly.
        while let Some(request) = requests.pop_front() {
            println!("answering request");
            request.answer(counter);
            println!("done answering");
            counter += 1;
        }

        sched().thread_yield();
    }
}

pub fn thread_2() -> ! {
    let mailbox = {
        loop {
            if let Some(value) = MAILBOX1.get() {
                break value;
            }
            sched().thread_yield();
        }
    };

    let mut counter = 0;
    loop {
        println!("sending request with counter {counter}");
        let res = mailbox.send(counter);
        println!("request sent");
        counter += 1;
        println!("waiting to receive (4s timeout)...");
        let value = res.receive_timeout(Duration::from_secs(4));
        println!("received, got value: {value:?}");

        sched().thread_sleep(Duration::from_secs(1));
    }
}

pub fn thread_kb_listener() -> ! {
    let rx = KEYBOARD_BROADCAST.subscribe();

    loop {
        let key = rx.recv_timeout(Duration::from_secs(4));

        if let Ok(key) = key {
            println!("Got key: {:?}", key);
        }
    }
}

pub fn draw() -> ! {
    println!("initializing");

    let info = screen_info();
    println!("Got screen info: {:?}", info);
    let x1 = 0;
    let y1 = 0;
    let x2 = 300;
    let y2 = 300;
    let mut counter = 0u64;

    loop {
        // Create rainbow effect (hue cycles 0-360 degrees)
        let hue = (counter * 5) % 360; // 5 degrees per frame
        let current_color = hsl_to_rgb(hue as f32, 1.0, 0.5);

        draw_rect(x1, y1, x2, y2, current_color);
        render();

        counter += 1;
        sched().thread_wait_timeout(Duration::from_millis(14));
    }
}
