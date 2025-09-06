use core::time::Duration;

use crate::{
    graphics::{
        api::{draw_rect, render, screen_info},
        colors::hsl_to_rgb,
    },
    println,
    thread::scheduler::sched,
};

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
