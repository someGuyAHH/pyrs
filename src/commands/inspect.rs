use anyhow::Result;

use crate::bundle::Bundle;
use crate::cli::InspectArgs;
use crate::manifest::EntryKind;

pub fn run(args: InspectArgs) -> Result<()> {
    let bundle = Bundle::open(&args.bundle)?;
    let m = &bundle.manifest;

    println!("{} {}", m.name, m.version);
    match m.entry.kind {
        EntryKind::ConsoleScript => println!("  entry:       {} -> {}", m.entry.name, m.entry.target),
        EntryKind::Module => println!("  entry:       python -m {}", m.entry.target),
    }
    println!("  python:      {}", m.requires_python.as_deref().unwrap_or("any version"));
    println!(
        "  built on:    {} / Python {} (pyrs {})",
        m.built_with.platform, m.built_with.python, m.built_with.pyrs
    );
    if m.pure {
        println!("  compiled:    no, but packages were resolved for the build platform");
    } else {
        println!("  compiled:    yes, only runs on a matching platform and Python version");
    }
    println!("  id:          {}", &bundle.digest[..16]);
    println!("  wheels ({}):", m.wheels.len());
    for wheel in &m.wheels {
        println!("    {}  {}", &wheel.sha256[..12.min(wheel.sha256.len())], wheel.file);
    }
    Ok(())
}
