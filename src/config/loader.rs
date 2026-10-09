use std::{
    error::Error,
    fmt,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use super::{Config, ConfigError};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum ConfigLoadError {
    Read { path: PathBuf, source: io::Error },
    Invalid { path: PathBuf, source: ConfigError },
    TooLarge { path: PathBuf },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigLoadError> {
        // preserve the path so reload failures identify the file
        let read_error = |source| ConfigLoadError::Read {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(read_error)?;
        let mut bytes = Vec::new();
        // read one extra byte to detect oversize input without trusting file metadata
        file.take(MAX_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigLoadError::TooLarge {
                path: path.to_path_buf(),
            });
        }
        let source = std::str::from_utf8(&bytes)
            .map_err(|source| read_error(io::Error::new(io::ErrorKind::InvalidData, source)))?;

        Self::parse(source).map_err(|source| ConfigLoadError::Invalid {
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
            Self::TooLarge { path } => write!(
                formatter,
                "config `{}` exceeds the {MAX_CONFIG_BYTES}-byte limit (1 MiB)",
                path.display()
            ),
        }
    }
}

impl Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Invalid { source, .. } => Some(source),
            Self::TooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{Config, ConfigLoadError, MAX_CONFIG_BYTES};

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

    #[test]
    fn load_accepts_a_valid_config_at_the_size_limit() {
        let mut source = "[[listener]]\ntimeout = 30\n".to_string();
        source.push_str(&" ".repeat(MAX_CONFIG_BYTES - source.len()));
        let file = TestConfigFile::new(&source);

        let config = match Config::load(file.path()) {
            Ok(config) => config,
            Err(error) => panic!("failed to load boundary config: {error}"),
        };

        assert_eq!(config.listeners[0].timeout_seconds, 30);
    }

    #[test]
    fn load_rejects_a_config_one_byte_over_the_limit() {
        let file = TestConfigFile::new(&" ".repeat(MAX_CONFIG_BYTES + 1));
        let error = match Config::load(file.path()) {
            Ok(config) => panic!("unexpected oversized config: {config:?}"),
            Err(error) => error,
        };

        assert!(matches!(&error, ConfigLoadError::TooLarge { path } if path == file.path()));
        assert!(error.to_string().contains("1048576-byte limit (1 MiB)"));
    }

    #[test]
    fn load_bounds_input_without_a_reported_file_size() {
        let path = Path::new("/dev/zero");

        assert!(matches!(
            Config::load(path),
            Err(ConfigLoadError::TooLarge { path: error_path }) if error_path == path
        ));
    }

    #[test]
    fn load_reports_invalid_utf8_as_a_read_error() {
        let file = TestConfigFile::new("");
        if let Err(error) = fs::write(file.path(), [0xff]) {
            panic!("failed to write invalid UTF-8: {error}");
        }

        assert!(matches!(
            Config::load(file.path()),
            Err(ConfigLoadError::Read { source, .. }) if source.kind() == io::ErrorKind::InvalidData
        ));
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
