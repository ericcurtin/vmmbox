//! vmmbox: hardware-accelerated Linux distro VMs that mirror your user account
//! and share your home directory, on top of QEMU.

mod checksum;
mod cloudinit;
mod commands;
mod cpu;
mod distro;
mod gui;
mod host;
mod http;
mod image;
mod paths;
mod proc;
mod qemu;
mod qmp;
mod resources;
mod ssh;
mod tools;
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
    /// Run a command in a running VM as your user, in your current directory.
    /// GUI apps (e.g. google-chrome) open as windows on your desktop.
    Exec {
        /// Open as a GUI app even if vmmbox doesn't recognise the command as one
        /// (needed to start GUI apps from a shell: `exec --gui ubuntu bash`)
        #[arg(long)]
        gui: bool,
        /// Never forward windows; run as a plain terminal command
        #[arg(long, conflicts_with = "gui")]
        no_gui: bool,
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
    /// Remove a VM and its disk (the pulled image is kept)
    Rm {
        /// Stop the VM first if it is running
        #[arg(short, long)]
        force: bool,
        /// Distro name of the VM
        name: ImageRef,
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
        Command::Exec {
            gui,
            no_gui,
            name,
            command,
        } => {
            let mode = match (gui, no_gui) {
                (true, _) => commands::GuiMode::Always,
                (_, true) => commands::GuiMode::Never,
                _ => commands::GuiMode::Auto,
            };
            commands::exec(&name, &command, mode)
        }
        Command::Images => commands::images().map(|()| 0),
        Command::Pull { image } => commands::pull(&image).map(|()| 0),
        Command::Ps { all } => commands::ps(all).map(|()| 0),
        Command::Rm { force, name } => commands::rm(&name, force).map(|()| 0),
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
