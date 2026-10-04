//! vmmbox: hardware-accelerated Linux distro VMs that mirror your user account
//! and share your home directory, on top of QEMU.

mod checksum;
mod cloudinit;
mod commands;
mod cpu;
mod distro;
mod host;
mod http;
mod image;
mod paths;
mod proc;
mod qemu;
mod qmp;
mod resources;
mod ssh;
mod util;
mod vm;

use clap::{Parser, Subcommand};
use distro::ImageRef;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "vmmbox", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the VM for a distro, creating it on first use (e.g. ubuntu, ubuntu:24.04, fedora:44)
    Start {
        /// Distro, optionally with a version: distro[:version]
        image: ImageRef,
    },
    /// Shut a VM down
    Stop {
        /// Distro name of the VM
        name: ImageRef,
    },
    /// Run a command in a running VM as your user, in your current directory
    Exec {
        /// Distro name of the VM
        name: ImageRef,
        /// Command and arguments to run
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List pulled images
    Images,
    /// Download a distro's cloud image (e.g. ubuntu, ubuntu:24.04, fedora:44)
    Pull {
        /// Distro, optionally with a version: distro[:version]
        image: ImageRef,
    },
    /// List VMs
    Ps {
        /// Show stopped VMs too
        #[arg(short, long)]
        all: bool,
    },
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Command::Start { image } => commands::start(&image).map(|()| 0),
        Command::Stop { name } => commands::stop(&name).map(|()| 0),
        Command::Exec { name, command } => commands::exec(&name, &command),
        Command::Images => commands::images().map(|()| 0),
        Command::Pull { image } => commands::pull(&image).map(|()| 0),
        Command::Ps { all } => commands::ps(all).map(|()| 0),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("vmmbox: {e:#}");
            ExitCode::FAILURE
        }
    }
}
