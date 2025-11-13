use clap::{Parser, Subcommand};
use xenstore_rs::{unix::XsUnix, Xs, XsPerm};

/// Demo/test tool for xenstore Rust bindings
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List Xenstore keys in path
    List {
        #[arg()]
        path: String,
    },
    /// Read value of Xenstore path
    Read {
        #[arg()]
        path: String,
    },
    /// Remove value of Xenstore path
    Rm {
        #[arg()]
        path: String,
    },
    /// Write value to Xenstore path
    Write {
        #[arg()]
        path: String,
        #[arg()]
        data: String,
    },
    /// Get node permissions.
    GetPerms {
        #[arg()]
        path: String,
    },
    /// Set node permissions
    SetPerms {
        #[arg()]
        path: String,
        #[arg()]
        perms: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    let mut xs = XsUnix::new().expect("xenstore should open");

    match cli.command {
        Command::List { path } => cmd_list(&mut xs, &path),
        Command::Read { path } => cmd_read(&mut xs, &path),
        Command::Rm { path } => cmd_rm(&mut xs, &path),
        Command::Write { path, data } => cmd_write(&mut xs, &path, &data),
        Command::GetPerms { path } => cmd_get_perms(&mut xs, &path),
        Command::SetPerms { path, perms } => cmd_set_perms(&mut xs, &path, &perms),
    }
}

fn cmd_list(xs: &mut impl Xs, path: &String) {
    let values = xs.directory(&path).expect("path should be readable");
    for value in values {
        println!("{}", value);
    }
}

fn cmd_read(xs: &mut impl Xs, path: &String) {
    let value = xs.read(&path).expect("path should be readable");
    println!("{}", value);
}

fn cmd_rm(xs: &mut impl Xs, path: &String) {
    xs.rm(&path).expect("cannot rm xenstore path");
}

fn cmd_write(xs: &mut impl Xs, path: &String, data: &String) {
    xs.write(&path, &data)
        .expect("cannot write to xenstore path");
}

fn cmd_get_perms(xs: &mut impl XsPerm, path: &String) {
    let perms = xs.get_perms(path).expect("path should be readable");

    for perm in perms {
        println!("{perm}");
    }
}

fn cmd_set_perms(xs: &mut impl XsPerm, path: &String, perms: &[String]) {
    let perms = perms
        .iter()
        .map(|s| s.parse())
        .collect::<Result<Vec<_>, _>>()
        .expect("Unable to parse permissions");

    xs.set_perms(path, &perms)
        .expect("Unable to set permissions");
}
