#![expect(unused)]

use core::time::Duration;

use alloc::vec::Vec;
use spin::Once;
use x86_64::instructions::hlt;

use crate::thread::mailbox::Mailbox;

pub(super) static REQUESTS: Once<Mailbox<Request, Response>> = Once::new();

fn send_request(request: Request) -> Response {
    let requests = {
        loop {
            if let Some(req) = REQUESTS.poll() {
                break req;
            }
            hlt();
        }
    };

    let response = requests.send(request);

    loop {
        match response.receive_timeout(Duration::from_millis(200)) {
            Ok(res) => break res,
            Err(_) => continue,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) enum Request {
    Draw(DrawRequest),
    ScreenInfo,
    Render,
}

#[derive(Debug, Clone)]
pub(super) enum Response {
    ScreenInfo(ScreenInfo),
    Ok,
}

#[derive(Debug, Clone)]
pub struct DrawRequest {
    // rr_gg_bb_aa 0xff_ff_ff_ff
    pub pixels: Vec<u32>,
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
}

pub fn screen_info() -> ScreenInfo {
    let Response::ScreenInfo(screen_info) = send_request(Request::ScreenInfo) else {
        unreachable!()
    };
    screen_info
}

pub fn render() {
    let Response::Ok = send_request(Request::Render) else {
        unreachable!()
    };
}

pub fn draw(request: DrawRequest) {
    let Response::Ok = send_request(Request::Draw(request)) else {
        unreachable!()
    };
}

pub fn draw_rect(x: u64, y: u64, width: u64, height: u64, color: u32) {
    if width == 0 || height == 0 {
        return;
    }

    let pixels = alloc::vec![color; (width * height) as usize];

    let request = DrawRequest {
        pixels,
        x,
        y,
        width,
        height,
    };

    draw(request);
}

pub fn draw_line(x1: u64, y1: u64, x2: u64, y2: u64, color: u32) {
    // Calculate bounding rectangle
    let min_x = x1.min(x2);
    let max_x = x1.max(x2);
    let min_y = y1.min(y2);
    let max_y = y1.max(y2);

    let width = max_x - min_x + 1;
    let height = max_y - min_y + 1;

    // Create transparent buffer
    let mut pixels = alloc::vec![crate::graphics::colors::TRANSPARENT; (width * height) as usize];

    // Bresenham's line algorithm
    let dx = (x2 as i64 - x1 as i64).abs();
    let dy = (y2 as i64 - y1 as i64).abs();
    let sx = if x1 < x2 { 1 } else { -1 };
    let sy = if y1 < y2 { 1 } else { -1 };
    let mut err = dx - dy;

    let mut x = x1 as i64;
    let mut y = y1 as i64;

    loop {
        // Convert to buffer coordinates
        let buf_x = (x as u64 - min_x) as usize;
        let buf_y = (y as u64 - min_y) as usize;
        let index = buf_y * width as usize + buf_x;

        if index < pixels.len() {
            pixels[index] = color;
        }

        if x == x2 as i64 && y == y2 as i64 {
            break;
        }

        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }

    let request = DrawRequest {
        pixels,
        x: min_x,
        y: min_y,
        width,
        height,
    };

    draw(request);
}
