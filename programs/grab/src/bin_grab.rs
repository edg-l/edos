//! grab - install software from the EDOS package repository.

use grab::{Progress, db};
use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

const USAGE: &str = "\
usage: grab [command] [arguments]

  grab NAME...              install, the short form of `grab install`
  grab install NAME...      install packages
  grab install --allow-unsigned FILE.tar.gz
                            install a local archive, checking no signature
  grab remove NAME...       remove packages
  grab upgrade [NAME...]    install newer versions, all packages if none named
  grab update               refresh the package list
  grab search TERM          search names and summaries
  grab show NAME            everything known about one package
  grab list [--installed]   list packages
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{}", USAGE);
        return ExitCode::SUCCESS;
    }

    let (command, rest) = (args[0].as_str(), &args[1..]);
    let mut report = Report::new();

    let result = match command {
        "install" => install(rest, &mut report),
        "remove" => remove(rest, &mut report),
        "upgrade" => upgrade(rest, &mut report),
        "update" => grab::update(&mut report).map(|_| ()),
        "search" => search(rest),
        "show" => show(rest, &mut report),
        "list" => list(rest, &mut report),
        // Anything else is taken as a package name, so `grab snake` works.
        _ => install(&args, &mut report),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            report.clear();
            eprintln!("grab: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn install(args: &[String], report: &mut Report) -> grab::Result<()> {
    let unsigned = args.iter().any(|a| a == "--allow-unsigned");
    let names: Vec<&String> = args
        .iter()
        .filter(|a| a.as_str() != "--allow-unsigned")
        .collect();

    if names.is_empty() {
        return Err(grab::Error::NotFound(
            "nothing named to install".to_string(),
        ));
    }
    for name in names {
        if unsigned {
            grab::install_local(name, report)?;
        } else {
            grab::install(name, report)?;
        }
    }
    Ok(())
}

fn remove(names: &[String], report: &mut Report) -> grab::Result<()> {
    if names.is_empty() {
        return Err(grab::Error::NotFound("nothing named to remove".to_string()));
    }
    for name in names {
        grab::remove(name, report)?;
    }
    Ok(())
}

fn upgrade(names: &[String], report: &mut Report) -> grab::Result<()> {
    let changed = grab::upgrade(names, report)?;
    if changed == 0 {
        println!("everything is up to date");
    }
    Ok(())
}

fn search(terms: &[String]) -> grab::Result<()> {
    let index = grab::index(&mut grab::Silent)?;
    let needle = terms.join(" ").to_lowercase();

    let mut found = 0;
    for package in &index.packages {
        let haystack = format!("{} {}", package.name, package.summary).to_lowercase();
        if needle.is_empty() || haystack.contains(&needle) {
            println!(
                "{:<16} {:<8} {}",
                package.name, package.version, package.summary
            );
            found += 1;
        }
    }
    if found == 0 {
        println!("nothing matches {:?}", needle);
    }
    Ok(())
}

fn show(names: &[String], report: &mut Report) -> grab::Result<()> {
    let Some(name) = names.first() else {
        return Err(grab::Error::NotFound("nothing named to show".to_string()));
    };
    let index = grab::index(report)?;
    let package = index
        .get(name)
        .ok_or_else(|| grab::Error::NotFound(format!("no package named {}", name)))?;

    println!("Package:   {}", package.name);
    println!("Version:   {}", package.version);
    println!("Summary:   {}", package.summary);
    println!("Category:  {}", package.category);
    println!("Size:      {} bytes", package.size);
    println!("SHA256:    {}", package.sha256);
    println!("Installs:  {}", package.installs.join(" "));

    match db::read(name)? {
        Some(record) if record.version == package.version => {
            println!("Installed: {} (current)", record.version)
        }
        Some(record) => println!("Installed: {} (an upgrade is available)", record.version),
        None => println!("Installed: no"),
    }
    Ok(())
}

fn list(args: &[String], report: &mut Report) -> grab::Result<()> {
    if args.iter().any(|a| a == "--installed") {
        let records = db::installed()?;
        if records.is_empty() {
            println!("nothing is installed");
        }
        for record in records {
            println!(
                "{:<16} {:<8} {}",
                record.name, record.version, record.summary
            );
        }
        return Ok(());
    }

    let index = grab::index(report)?;
    for package in &index.packages {
        let mark = match db::read(&package.name)? {
            Some(record) if record.version == package.version => "*",
            Some(_) => "^",
            None => " ",
        };
        println!(
            "{} {:<16} {:<8} {}",
            mark, package.name, package.version, package.summary
        );
    }
    Ok(())
}

/// Progress on the terminal: messages on their own lines, transfers rewritten
/// in place so a download does not scroll the screen.
struct Report {
    transfer_shown: bool,
}

impl Report {
    fn new() -> Self {
        Report {
            transfer_shown: false,
        }
    }

    /// Take a transfer line back off the terminal before anything else is
    /// printed, so a message is never appended to a half-finished counter.
    fn clear(&mut self) {
        if self.transfer_shown {
            eprint!("\r{:60}\r", "");
            let _ = io::stderr().flush();
            self.transfer_shown = false;
        }
    }
}

impl Progress for Report {
    fn message(&mut self, text: &str) {
        self.clear();
        println!("{}", text);
    }

    fn transfer(&mut self, done: u64, total: Option<u64>) {
        match total {
            Some(total) if total > 0 => {
                eprint!("\r  {} / {} bytes ({}%)  ", done, total, done * 100 / total)
            }
            _ => eprint!("\r  {} bytes  ", done),
        }
        let _ = io::stderr().flush();
        self.transfer_shown = true;
    }
}
