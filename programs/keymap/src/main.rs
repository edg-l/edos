//! Show or set the keyboard layout.
//!
//! The layout is what turns a physical key into a character, and until it
//! matches the board in front of you the machine types the wrong symbols. With
//! no argument this reports what is in force and where that came from; with a
//! layout name it records the choice in `/etc/keymap`.
//!
//! A program decodes with the layout it resolved when it started, so a change
//! reaches programs started after it and not the ones already running.

use std::process::ExitCode;

use edos_lib::{
    config,
    keymap::{CONFIG_PATH, LAYOUTS, configured_layout, current_layout, layout_by_name},
    procinfo::boot_param,
};

const USAGE: &str = "usage: keymap [LAYOUT]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(arg) = args.next() else {
        report();
        return ExitCode::SUCCESS;
    };

    match arg.as_str() {
        "-h" | "--help" => {
            println!("{USAGE}");
            println!("\nWith no argument, report the layout in force. With a layout");
            println!("name, record it in {CONFIG_PATH}.\n");
            list();
            ExitCode::SUCCESS
        }
        name if name.starts_with('-') => {
            eprintln!("keymap: unknown option '{name}'\n{USAGE}");
            ExitCode::from(1)
        }
        name => set(name),
    }
}

fn report() {
    let active = current_layout();
    let source = if let Some(name) = boot_param("keymap") {
        if layout_by_name(&name).is_some() {
            "kernel command line".to_string()
        } else {
            format!("built-in default ({name} on the command line is not a layout)")
        }
    } else if let Some(name) = configured_layout() {
        if layout_by_name(&name).is_some() {
            CONFIG_PATH.to_string()
        } else {
            format!("built-in default ({name} in {CONFIG_PATH} is not a layout)")
        }
    } else {
        "built-in default".to_string()
    };

    println!("{}  {}", active.name, active.description);
    println!("from {source}");
    println!();
    list();
}

fn list() {
    println!("available layouts:");
    for layout in LAYOUTS {
        println!("  {:<4}{}", layout.name, layout.description);
    }
}

fn set(name: &str) -> ExitCode {
    let Some(layout) = layout_by_name(name) else {
        eprintln!("keymap: no layout named '{name}'");
        list();
        return ExitCode::from(1);
    };

    // The comment goes in the file because this is the one setting whose own
    // effect can stop you editing the file that carries it.
    let comment = "Keyboard layout. `keymap` writes this; `keymap=NAME` on the kernel\n\
                   command line overrides it, which is the way back from a layout\n\
                   that makes this file uneditable.";
    if let Err(err) = config::write(CONFIG_PATH, layout.name, comment) {
        eprintln!("keymap: {CONFIG_PATH}: {err}");
        return ExitCode::from(1);
    }

    println!("{}  {}", layout.name, layout.description);
    println!("recorded in {CONFIG_PATH}");
    println!("programs already running keep the layout they started with");
    ExitCode::SUCCESS
}
