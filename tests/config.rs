use std::{error::Error, fs, process::Command};

use tempfile::TempDir;

fn config_home() -> Result<TempDir, Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    fs::create_dir_all(home.path().join(".config/sweetsidle"))?;
    fs::write(
        home.path().join(".config/sweetsidle/config.toml"),
        "[[listener]]\ntimeout = 30\n",
    )?;
    Ok(home)
}

#[test]
fn empty_home_does_not_load_config_from_the_working_directory() -> Result<(), Box<dyn Error>> {
    let directory = config_home()?;
    let output = Command::new(env!("CARGO_BIN_EXE_sweetsidle"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_sweetsidle"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_sweetsidle"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_sweetsidle"))
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_sweetsidle"));
        command
            .args([
                "--check-config",
                "--config",
                ".config/sweetsidle/config.toml",
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
