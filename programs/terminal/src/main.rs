#![no_std]
#![no_main]

use elibc::{
    graphics::{Color, FontWeight, RasterHeight, Screen, TextStyle},
    println,
};

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let screen = Screen::get().unwrap();

    // Clear the screen
    screen.fill(Color::BLACK).unwrap();

    // Create text styles
    let white_style = TextStyle::new(Color::WHITE).with_size(RasterHeight::Size24);

    let red_style = TextStyle::new(Color::RED)
        .with_size(RasterHeight::Size32)
        .with_weight(FontWeight::Bold);

    let green_style = TextStyle::new(Color::GREEN)
        .with_size(RasterHeight::Size24)
        .with_background(Color::BLUE);

    // Demonstrate text rendering
    screen
        .draw_text(50, 50, "Hello, World!", &white_style)
        .unwrap();
    screen
        .draw_text(50, 100, "edos Kernel text Demo", &red_style)
        .unwrap();
    screen
        .draw_text(50, 160, "Text with background", &green_style)
        .unwrap();

    // Demonstrate character rendering
    screen.draw_char(50, 220, '🦀', &white_style).unwrap_or(()); // Rust crab emoji (may not render)
    screen.draw_char(80, 220, 'A', &red_style).unwrap();
    screen.draw_char(110, 220, 'B', &white_style).unwrap();
    screen.draw_char(140, 220, 'C', &white_style).unwrap();

    // Demonstrate wrapped text
    let wrap_style = TextStyle::new(Color::CYAN).with_size(RasterHeight::Size24);
    let long_text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.";
    screen
        .draw_text_wrapped(50, 280, long_text, &wrap_style, 600)
        .unwrap();

    // Multi-line text demonstration
    let multiline_text = "Line 1\nLine 2\nrust > C\nchiller is a botter";
    screen
        .draw_text(50, 600, multiline_text, &white_style)
        .unwrap();

    // Draw a decorative rectangle
    screen.draw_rect(700, 20, 50, 60, Color::RED).unwrap();

    screen.render().unwrap();

    println!("Text rendering demonstration complete!");
    0
}
