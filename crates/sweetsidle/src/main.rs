const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        None | Some("-h" | "--help") => print_help(),
        Some("-V" | "--version") => println!("sweetsidle {VERSION}"),
        Some(arg) => {
            eprintln!("error: unknown argument `{arg}`");
            eprintln!("try `sweetsidle --help`");
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "\
sweetsidle {VERSION}

Usage: sweetsidle [OPTIONS]

Options:
  -h, --help     Print help
  -V, --version  Print version
"
    );
}
