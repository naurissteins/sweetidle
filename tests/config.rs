use std::{error::Error, ffi::OsStr, fs, os::unix::ffi::OsStrExt, process::Command};

use tempfile::TempDir;

fn config_home() -> Result<TempDir, Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    fs::create_dir_all(home.path().join(".config/sweetidle"))?;
    fs::write(
        home.path().join(".config/sweetidle/config.toml"),
        "[[listener]]\ntimeout = 30\n",
    )?;
    Ok(home)
}

#[test]
fn empty_home_does_not_load_config_from_the_working_directory() -> Result<(), Box<dyn Error>> {
    let directory = config_home()?;
    let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .arg("--check-config")
        .env("HOME", "")
        .current_dir(directory.path())
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr)?,
        "error: HOME must be absolute; pass --config <PATH>\n"
    );
    Ok(())
}

#[test]
fn relative_home_does_not_load_config_from_the_working_directory() -> Result<(), Box<dyn Error>> {
    let directory = config_home()?;
    let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .arg("--check-config")
        .env("HOME", ".")
        .current_dir(directory.path())
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr)?,
        "error: HOME must be absolute; pass --config <PATH>\n"
    );
    Ok(())
}

#[test]
fn missing_home_reports_how_to_select_a_config() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .arg("--check-config")
        .env_remove("HOME")
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr)?,
        "error: HOME is not set; pass --config <PATH>\n"
    );
    Ok(())
}

#[test]
fn absolute_home_loads_the_default_config() -> Result<(), Box<dyn Error>> {
    let home = config_home()?;
    let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .arg("--check-config")
        .env("HOME", home.path())
        .output()?;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "configuration ok: 1 listener(s), idle hint disabled\n"
    );
    Ok(())
}

#[test]
fn explicit_relative_config_does_not_require_valid_home() -> Result<(), Box<dyn Error>> {
    let directory = config_home()?;
    for home in [None, Some(""), Some("relative")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sweetidle"));
        command
            .args([
                "--check-config",
                "--config",
                ".config/sweetidle/config.toml",
            ])
            .current_dir(directory.path());
        if let Some(home) = home {
            command.env("HOME", home);
        } else {
            command.env_remove("HOME");
        }
        let output = command.output()?;

        assert!(output.status.success(), "HOME={home:?}: {output:?}");
    }
    Ok(())
}

#[test]
fn non_utf8_config_path_is_loaded_unchanged() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(OsStr::from_bytes(b"idle-\xff.toml"));
    fs::write(&path, "[[listener]]\ntimeout = 30\n")?;
    let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .arg("--check-config")
        .arg("--config")
        .arg(&path)
        .output()?;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "configuration ok: 1 listener(s), idle hint disabled\n"
    );
    Ok(())
}

#[test]
fn non_utf8_unknown_argument_is_a_usage_error() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_sweetidle"))
        .arg(OsStr::from_bytes(b"--bad-\xff"))
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr)?,
        "error: unknown argument `--bad-\u{fffd}`\ntry `sweetidle --help`\n"
    );
    Ok(())
}
