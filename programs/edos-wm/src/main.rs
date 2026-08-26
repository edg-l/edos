//! EDOS Window Manager - User-space compositor.

mod compositor;
mod cursor;
mod decorations;
mod desktop_menu;
mod dirty;
mod frametime;
mod input;
mod interaction;
mod session;

fn main() {
    eprintln!("[wm] starting");
    match session::Session::open() {
        Ok(mut session) => session.run(),
        Err(e) => eprintln!("Failed to initialize screen: {e}"),
    }
}
