use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use super::{Config, ConfigError};

#[derive(Debug)]
pub enum ConfigLoadError {
    Read { path: PathBuf, source: io::Error },
    Invalid { path: PathBuf, source: ConfigError },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigLoadError> {
        // preserve the path in either error so reload failures identify the file
        let source = fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        Self::parse(&source).map_err(|source| ConfigLoadError::Invalid {
            path: path.to_path_buf(),
            source,
        })
    }
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "failed to read `{}`: {source}", path.display())
            }
            Self::Invalid { path, source } => {
                write!(formatter, "invalid config `{}`: {source}", path.display())
            }
        }
    }
}

impl Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Invalid { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{Config, ConfigLoadError};

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn load_reads_and_parses_a_valid_file() {
        let file = TestConfigFile::new("[[listener]]\ntimeout = 30\n");

        let config = match Config::load(file.path()) {
            Ok(config) => config,
            Err(error) => panic!("failed to load test config: {error}"),
        };

        assert_eq!(config.listeners[0].timeout_seconds, 30);
    }

    #[test]
    fn load_reports_invalid_file_without_producing_a_config() {
        let file = TestConfigFile::new("[[listener]]\ntimeout = 0\n");

        let error = match Config::load(file.path()) {
            Ok(config) => panic!("unexpected config: {config:?}"),
            Err(error) => error,
        };

        assert!(matches!(error, ConfigLoadError::Invalid { .. }));
    }

    struct TestConfigFile {
        path: PathBuf,
    }

    impl TestConfigFile {
        fn new(source: &str) -> Self {
            let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sweetsidle-config-test-{}-{id}.toml",
                std::process::id()
            ));
            if let Err(error) = fs::write(&path, source) {
                panic!("failed to write test config: {error}");
            }

            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestConfigFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
