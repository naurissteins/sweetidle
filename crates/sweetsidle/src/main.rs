const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(error) = run(std::env::args().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut check_config = false;
    let mut config_path = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("sweetsidle {VERSION}");
                return Ok(());
            }
            "--check-config" => check_config = true,
            "-c" | "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| format!("`{arg}` requires a path argument"))?;

                config_path = Some(std::path::PathBuf::from(path));
            }
            _ => return Err(format!("unknown argument `{arg}`\ntry `sweetsidle --help`")),
        }
    }

    if check_config {
        check_config_file(config_path)?;
    } else {
        print_help();
    }

    Ok(())
}

fn print_help() {
    println!(
        "\
sweetsidle {VERSION}

Usage: sweetsidle [OPTIONS]

Options:
  -c, --config <PATH>  Use a specific config file
      --check-config   Parse and validate the config file, then exit
  -h, --help           Print help
  -V, --version        Print version
"
    );
}

fn check_config_file(path: Option<std::path::PathBuf>) -> Result<(), String> {
    let path = match path {
        Some(path) => path,
        None => default_config_path()?,
    };

    let source = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read `{}`: {error}", path.display()))?;
    let config = sweetsidle_config::Config::parse(&source)
        .map_err(|error| format!("invalid config `{}`: {error}", path.display()))?;

    println!("configuration ok: {} listener(s)", config.listeners.len());
    Ok(())
}

fn default_config_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; pass --config <PATH>".to_string())?;

    Ok(std::path::PathBuf::from(home).join(".config/sweetsidle/config.kdl"))
}
