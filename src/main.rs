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
    /// Run a command in a VM as your user, in your current directory. The VM is
    /// pulled, created and started first if need be. GUI apps (e.g.
    /// google-chrome) open as windows on your desktop.
    // `exec` was this command's name in 0.1.x; it keeps working, unlisted.
    #[command(alias = "exec")]
    Run {
        /// Open as a GUI app even if vmmbox doesn't recognise the command as one
        /// (needed to start GUI apps from a shell: `run --gui ubuntu bash`)
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
    /// Remove a pulled image (VMs made from it are unaffected)
    Rmi {
        /// Distro, optionally with a version: distro[:version]
        image: ImageRef,
    },
    /// Remove a VM and its disk (the pulled image is kept; see `rmi`)
    Rm {
        /// Stop the VM first if it is running
        #[arg(short, long)]
        force: bool,
        /// Distro name of the VM
        name: ImageRef,
    },
    /// List the VMs you have created, running or not
    #[command(visible_alias = "list")]
    Ls,
    /// List running VMs
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
        Command::Run {
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
            commands::run(&name, &command, mode)
        }
        Command::Images => commands::images().map(|()| 0),
        Command::Pull { image } => commands::pull(&image).map(|()| 0),
        Command::Ls => commands::ps(true).map(|()| 0),
        Command::Ps { all } => commands::ps(all).map(|()| 0),
        Command::Rmi { image } => commands::rmi(&image).map(|()| 0),
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        let mut argv = vec!["vmmbox"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap().command
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        // Catches duplicate names and aliases.
        Cli::command().debug_assert();
    }

    #[test]
    fn ls_and_list_both_list_created_vms() {
        assert!(matches!(parse(&["ls"]), Command::Ls));
        assert!(matches!(parse(&["list"]), Command::Ls));
    }

    #[test]
    fn run_takes_a_vm_and_a_command() {
        let Command::Run {
            gui,
            no_gui,
            name,
            command,
        } = parse(&["run", "ubuntu:24.04", "ls", "-la"])
        else {
            panic!("not run");
        };
        assert!(!gui && !no_gui);
        assert_eq!(name.distro.name, "ubuntu");
        assert_eq!(command, ["ls", "-la"]);
        assert!(matches!(
            parse(&["run", "--gui", "ubuntu", "bash"]),
            Command::Run { gui: true, .. }
        ));
    }

    #[test]
    fn exec_still_works_as_the_old_name_but_is_not_advertised() {
        assert!(matches!(
            parse(&["exec", "ubuntu", "bash"]),
            Command::Run { .. }
        ));
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("run "), "{help}");
        assert!(!help.contains("exec"), "{help}");
    }

    #[test]
    fn ps_still_lists_only_running_vms_unless_asked() {
        assert!(matches!(parse(&["ps"]), Command::Ps { all: false }));
        assert!(matches!(parse(&["ps", "-a"]), Command::Ps { all: true }));
        assert!(matches!(parse(&["ps", "--all"]), Command::Ps { all: true }));
    }

    #[test]
    fn images_and_rmi_take_the_forms_pull_does() {
        assert!(matches!(parse(&["images"]), Command::Images));
        assert!(matches!(
            parse(&["rmi", "ubuntu:24.04"]),
            Command::Rmi { .. }
        ));
        assert!(matches!(parse(&["rmi", "fedora"]), Command::Rmi { .. }));
    }
}
