use std::env;
use std::fs;
use std::io::{self, Write};

use edos_lib::http::{self, Url};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut output: Option<&str> = None;
    let mut url_arg: Option<&str> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-O" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("wget: -O requires an argument");
                    std::process::exit(1);
                }
                output = Some(&args[i]);
            }
            arg if arg.starts_with("-O") => {
                output = Some(&args[i][2..]);
            }
            _ => url_arg = Some(&args[i]),
        }
        i += 1;
    }

    let Some(url_arg) = url_arg else {
        eprintln!("usage: wget [-O output] <url>");
        std::process::exit(1);
    };

    let url = match Url::parse(url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("wget: {}: {}", url_arg, e);
            std::process::exit(1);
        }
    };

    let response = match http::get(&url) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wget: {}:{}: {}", url.host, url.port, e);
            std::process::exit(1);
        }
    };

    let dest = output.unwrap_or_else(|| url.filename());

    if dest == "-" {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(&response.body);
        let _ = out.flush();
    } else {
        if let Err(e) = fs::write(dest, &response.body) {
            eprintln!("wget: {}: {}", dest, e);
            std::process::exit(1);
        }
        eprintln!("wget: saved to '{}' ({} bytes)", dest, response.body.len());
    }
}
