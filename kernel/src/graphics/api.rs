#![expect(unused)]

use core::time::Duration;

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
    ScreenInfo,
    Render,
}

pub(super) enum Response {
    ScreenInfo(ScreenInfo),
    Rendered,
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
    let Response::Rendered = send_request(Request::Render) else {
        unreachable!()
    };
}
