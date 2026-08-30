use clap::Parser;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Validation result for a user-provided path.
pub enum PathValidateResult {
    /// The path is valid and resolves within the watched directory.
    Valid(PathBuf),
    /// The path escapes the watched directory after symlink resolution.
    OutsideDirectory,
    /// The path does not exist.
    NotFound,
}

/// Default values
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_MAX_FILE_SIZE_MB: u64 = 20;
const DEFAULT_BATCH_SIZE: usize = 500;
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 1000;

/// MCP transport mode.
#[derive(Debug)]
pub enum TransportMode {
    Http,
    Stdio,
}

/// Merged configuration from config file and CLI arguments.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory to index. Canonical (absolute, symlinks resolved) after
    /// [`Config::canonicalize_paths()`].
    pub directory: PathBuf,
    /// Tantivy index location. Canonical after [`Config::canonicalize_paths()`].
    pub index_path: PathBuf,
    pub port: u16,
    pub bind: String,
    pub max_file_size_mb: u64,
    pub batch_size: usize,
    pub batch_timeout_ms: u64,
    /// Allowed extensions for indexing. If empty, all files are indexed.
    pub allowed_extensions: Vec<String>,
}

/// Configuration parsed from a TOML file.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub directory: Option<PathBuf>,
    pub index_path: Option<PathBuf>,
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub max_file_size_mb: Option<u64>,
    pub batch_size: Option<usize>,
    pub batch_timeout_ms: Option<u64>,
    pub allowed_extensions: Option<Vec<String>>,
}

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(name = "file_indexr", about = "Local file search engine")]
pub struct Args {
    /// Directory to index
    #[arg(short, long)]
    pub directory: Option<PathBuf>,

    /// Path to store the index
    #[arg(short = 'i', long)]
    pub index_path: Option<PathBuf>,

    /// HTTP port to listen on (ignored with --stdio)
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Bind address (ignored with --stdio)
    #[arg(short, long)]
    pub bind: Option<String>,

    /// Max file size to index content (in MB)
    #[arg(long, value_name = "MB")]
    pub max_file_size_mb: Option<u64>,

    /// Number of changes per batch commit
    #[arg(long)]
    pub batch_size: Option<usize>,

    /// Max wait time before committing batch (ms)
    #[arg(long, value_name = "MS")]
    pub batch_timeout_ms: Option<u64>,

    /// Path to config file
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Comma-separated list of allowed extensions (e.g., "md,txt,html"). If empty, all files are indexed.
    #[arg(long, value_delimiter = ',')]
    pub allowed_extensions: Option<Vec<String>>,

    /// Enable debug logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Run MCP over STDIO instead of HTTP server
    #[arg(long)]
    pub stdio: bool,
}

/// Load configuration from a TOML file.
pub async fn load_config_file(path: &Path) -> Result<ConfigFile, String> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format_io_error("read config file", path, &e))?;
    let config: ConfigFile = toml::from_str(&content)
        .map_err(|e| format!("Failed to parse config file {}: {}", path.display(), e))?;
    Ok(config)
}

/// Normalize a list of allowed extensions: trim whitespace, strip any leading
/// dots, lowercase, drop empty entries and de-duplicate (order preserved).
/// clap splits `--allowed-extensions` on commas without trimming, so
/// `"md, txt"` would otherwise yield a literal `" txt"` that never matches
/// `Path::extension()` and would silently skip indexing `.txt` files.
fn normalize_extensions(raw: Vec<String>) -> Vec<String> {
    // The list is short, so a linear scan is fine and avoids both a
    // per-entry clone and a HashSet allocation.
    let mut out = Vec::new();
    for entry in raw {
        let normalized = entry.trim().trim_start_matches('.').to_lowercase();
        if !normalized.is_empty() && !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out
}

/// First available value: CLI override, then config file, then default.
fn pick<T>(cli: Option<T>, file: Option<T>, default: T) -> T {
    cli.or(file).unwrap_or(default)
}

/// Format an I/O error with the action it failed on and the path involved.
fn format_io_error(action: &str, path: &Path, error: &std::io::Error) -> String {
    format!("Failed to {action} {}: {}", path.display(), error)
}

/// Merge config file with CLI overrides and produce final Config + transport mode.
/// CLI arguments take precedence over config file values.
pub fn merge_config(
    args: Args,
    file_config: Option<ConfigFile>,
) -> Result<(Config, TransportMode), String> {
    // Own the file config (missing file -> all-None default) so each field
    // is read by value instead of through `.as_ref().and_then(..)` chains.
    let file_config = file_config.unwrap_or_default();

    // Determine directory (CLI > file > error)
    let directory = args
        .directory
        .or(file_config.directory)
        .ok_or("Directory must be specified via --directory or config file")?;

    let base_index_path = directory.join(".file_indexr").join("index");
    let index_path = pick(args.index_path, file_config.index_path, base_index_path);

    let port = pick(args.port, file_config.port, DEFAULT_PORT);
    let bind = pick(args.bind, file_config.bind, DEFAULT_BIND.to_string());
    let max_file_size_mb = pick(
        args.max_file_size_mb,
        file_config.max_file_size_mb,
        DEFAULT_MAX_FILE_SIZE_MB,
    );
    let batch_size = pick(args.batch_size, file_config.batch_size, DEFAULT_BATCH_SIZE);
    let batch_timeout_ms = pick(
        args.batch_timeout_ms,
        file_config.batch_timeout_ms,
        DEFAULT_BATCH_TIMEOUT_MS,
    );
    let allowed_extensions = normalize_extensions(pick(
        args.allowed_extensions,
        file_config.allowed_extensions,
        Vec::new(),
    ));

    let transport = if args.stdio {
        TransportMode::Stdio
    } else {
        TransportMode::Http
    };

    Ok((
        Config {
            directory,
            index_path,
            port,
            bind,
            max_file_size_mb,
            batch_size,
            batch_timeout_ms,
            allowed_extensions,
        },
        transport,
    ))
}

/// Validate configuration values.
impl Config {
    pub fn validate(&self) -> Result<(), String> {
        // Single stat: Path::exists() is defined as metadata().is_ok(), so a
        // failed metadata() is exactly the old "does not exist" case.
        let metadata = std::fs::metadata(&self.directory)
            .map_err(|_| format!("Directory does not exist: {}", self.directory.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "Path is not a directory: {}",
                self.directory.display()
            ));
        }
        if self.port == 0 {
            return Err("Port cannot be 0".to_string());
        }
        if self.max_file_size_mb == 0 {
            return Err("max_file_size_mb must be > 0".to_string());
        }
        if self.batch_size == 0 {
            return Err("batch_size must be > 0".to_string());
        }
        if self.batch_timeout_ms == 0 {
            return Err("batch_timeout_ms must be > 0".to_string());
        }
        Ok(())
    }

    /// Resolve `directory` and `index_path` to canonical absolute paths.
    /// Must be called after `validate()` and before the config is shared:
    /// the scanner and watcher exclude the index directory via lexical
    /// `starts_with` comparisons, which are fragile to symlinks, `..` and
    /// relative path forms (a mismatch would let the walk descend into the
    /// index and index its own segment files).
    pub async fn canonicalize_paths(&mut self) -> Result<(), String> {
        self.directory = tokio::fs::canonicalize(&self.directory)
            .await
            .map_err(|e| format_io_error("canonicalize directory", &self.directory, &e))?;

        // Create the index directory first: canonicalize requires the path to exist.
        tokio::fs::create_dir_all(&self.index_path)
            .await
            .map_err(|e| format_io_error("create index directory", &self.index_path, &e))?;
        self.index_path = tokio::fs::canonicalize(&self.index_path)
            .await
            .map_err(|e| format_io_error("canonicalize index path", &self.index_path, &e))?;

        Ok(())
    }

    /// Maximum allowed file size in bytes.
    pub fn max_file_size_bytes(&self) -> u64 {
        self.max_file_size_mb * 1_000_000
    }

    /// Returns `true` if the file with given extension should be indexed.
    /// Accepts `Option<&OsStr>` directly from `Path::extension()`.
    /// If `allowed_extensions` is set, only those are allowed; otherwise all pass through.
    pub fn should_index_extension(&self, ext: Option<&std::ffi::OsStr>) -> bool {
        if self.allowed_extensions.is_empty() {
            return true;
        }
        let Some(ext) = ext else {
            return false;
        };
        let ext_str = ext.to_string_lossy();
        if ext_str.is_ascii() {
            // Allowed extensions are pre-lowercased, so for ASCII input a
            // case-insensitive compare equals the old lowercase-then-equal
            // check without an allocation.
            self.allowed_extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&ext_str))
        } else {
            self.allowed_extensions.contains(&ext_str.to_lowercase())
        }
    }

    /// Validate that `rel_path` resolves within the watched directory.
    ///
    /// Rejects obviously malicious paths (``..``, absolute paths) immediately
    /// with ``OutsideDirectory``. For all other paths, uses ``canonicalize``
    /// to resolve symlinks before comparing against the watched directory.
    pub async fn validate_path(&self, rel_path: &str) -> PathValidateResult {
        // Quick rejection using Path::components — handles both / and \\ on
        // Windows, detects ParentDir (..) and RootDir (absolute paths).
        if std::path::Path::new(rel_path).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            return PathValidateResult::OutsideDirectory;
        }

        let full = self.directory.join(rel_path);

        // Canonicalize both paths for accurate comparison after symlink resolution
        let Ok(base_canonical) = tokio::fs::canonicalize(&self.directory).await else {
            return PathValidateResult::NotFound;
        };
        let Ok(resolved) = tokio::fs::canonicalize(&full).await else {
            return PathValidateResult::NotFound;
        };

        if resolved.starts_with(&base_canonical) {
            PathValidateResult::Valid(resolved)
        } else {
            PathValidateResult::OutsideDirectory
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn make_temp_config(content: &str) -> std::path::PathBuf {
        let path = testutil::unique_temp_dir("test").join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn make_temp_dir() -> PathBuf {
        testutil::unique_temp_dir("test_dir")
    }

    fn make_test_args(directory: Option<PathBuf>) -> Args {
        Args {
            directory,
            index_path: None,
            port: None,
            bind: None,
            max_file_size_mb: None,
            batch_size: None,
            batch_timeout_ms: None,
            config: None,
            allowed_extensions: None,
            verbose: false,
            stdio: false,
        }
    }

    #[tokio::test]
    async fn test_load_config_file_valid() {
        let path = make_temp_config(
            r#"
directory = "/tmp"
port = 9090
bind = "0.0.0.0"
max_file_size_mb = 5
"#,
        );
        let config = load_config_file(&path).await.unwrap();
        assert_eq!(config.directory.as_ref().unwrap(), Path::new("/tmp"));
        assert_eq!(config.port, Some(9090));
        assert_eq!(config.bind, Some("0.0.0.0".to_string()));
        assert_eq!(config.max_file_size_mb, Some(5));
    }

    #[tokio::test]
    async fn test_load_config_file_empty() {
        let path = make_temp_config("");
        let config = load_config_file(&path).await.unwrap();
        assert!(config.directory.is_none());
        assert!(config.port.is_none());
    }

    #[tokio::test]
    async fn test_load_config_file_missing() {
        let result = load_config_file(Path::new("/nonexistent/config.toml")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_config_file_invalid_toml() {
        let path = make_temp_config("not [[valid toml");
        let result = load_config_file(&path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_config_file_unknown_field() {
        // A typo'd key must be rejected, not silently dropped.
        let path = make_temp_config(
            r#"
directory = "/tmp"
max_file_sise_mb = 50
"#,
        );
        let result = load_config_file(&path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_merge_cli_overrides_file() {
        let temp_dir = make_temp_dir();
        let path = make_temp_config(&format!(
            r#"
directory = "{}"
port = 9090
bind = "0.0.0.0"
"#,
            temp_dir.display()
        ));

        let mut args = make_test_args(None);
        args.index_path = Some(PathBuf::from("/custom/index"));
        args.port = Some(8080);
        args.bind = Some("127.0.0.1".to_string());

        let file_config = load_config_file(&path).await.ok();
        let (config, _) = merge_config(args, file_config).unwrap();

        // Directory from file
        assert_eq!(config.directory, temp_dir);
        // Index path from CLI overrides default
        assert_eq!(config.index_path, PathBuf::from("/custom/index"));
        // Port from CLI overrides file
        assert_eq!(config.port, 8080);
        // Bind from CLI overrides file
        assert_eq!(config.bind, "127.0.0.1");
    }

    #[test]
    fn test_merge_directory_from_cli() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));

        let (config, _) = merge_config(args, None).unwrap();
        assert_eq!(config.directory, temp_dir);
        // Default index path is <directory>/.file_indexr/index
        assert_eq!(
            config.index_path,
            temp_dir.join(".file_indexr").join("index")
        );
    }

    #[test]
    fn test_merge_no_directory_errors() {
        let args = make_test_args(None);

        let result = merge_config(args, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Directory must be specified"));
    }

    #[test]
    fn test_validate_missing_directory() {
        let _temp_dir = make_temp_dir();
        let args = make_test_args(Some(PathBuf::from("/nonexistent_dir_xyz")));

        let (config, _) = merge_config(args, None).unwrap();
        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_valid() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));

        let (config, _) = merge_config(args, None).unwrap();
        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn test_canonicalize_paths_resolves_dotdot() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (mut config, _) = merge_config(args, None).unwrap();

        // Re-express the directory in a non-canonical form containing `..`
        let base = temp_dir.parent().unwrap();
        config.directory = base
            .join("..")
            .join(base.file_name().unwrap())
            .join(temp_dir.file_name().unwrap());

        config.canonicalize_paths().await.unwrap();

        // Both paths must end up in the same lexical space: a file path
        // built by walking config.directory must match config.index_path
        // via `starts_with` (the scanner/watcher index-exclusion check).
        assert_eq!(config.directory, std::fs::canonicalize(&temp_dir).unwrap());
        assert!(config.index_path.exists());
        assert!(
            config
                .directory
                .join(".file_indexr/index/0.mfd")
                .starts_with(&config.index_path)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_canonicalize_paths_resolves_symlink_directory() {
        let real_dir = make_temp_dir();
        // Derive the link name from real_dir's name: unique_temp_dir
        // guarantees that name is fresh, so the derived link name cannot
        // collide with leftovers from previous runs (pid reuse).
        let link = real_dir.with_file_name(format!(
            "{}_link",
            real_dir.file_name().unwrap().to_string_lossy()
        ));
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        let args = make_test_args(Some(link));
        let (mut config, _) = merge_config(args, None).unwrap();

        config.canonicalize_paths().await.unwrap();

        // The symlinked watched dir must resolve to the real location, and
        // the index-exclusion comparison must hold in the resolved space.
        assert_eq!(config.directory, std::fs::canonicalize(&real_dir).unwrap());
        assert!(
            config
                .directory
                .join(".file_indexr/index/0.mfd")
                .starts_with(&config.index_path)
        );
    }

    #[tokio::test]
    async fn test_canonicalize_paths_custom_non_hidden_index() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (mut config, _) = merge_config(args, None).unwrap();

        // Custom non-hidden index dir inside the watched dir (the default
        // `.file_indexr` is masked by the hidden-component check anyway),
        // plus a `..` in the directory form so the raw forms diverge.
        config.index_path = temp_dir.join("idx").join("data");
        let base = temp_dir.parent().unwrap();
        config.directory = base
            .join("..")
            .join(base.file_name().unwrap())
            .join(temp_dir.file_name().unwrap());

        config.canonicalize_paths().await.unwrap();

        assert!(config.index_path.is_dir());
        assert!(
            config
                .directory
                .join("idx/data/0.mfd")
                .starts_with(&config.index_path)
        );
    }

    #[tokio::test]
    async fn test_allowed_extensions_from_config() {
        let temp_dir = make_temp_dir();
        let path = make_temp_config(&format!(
            r#"
directory = "{}"
allowed_extensions = ["md", "txt", "html"]
"#,
            temp_dir.display()
        ));

        let args = make_test_args(None);

        let file_config = load_config_file(&path).await.ok();
        let (config, _) = merge_config(args, file_config).unwrap();
        assert_eq!(config.allowed_extensions.len(), 3);
        assert!(config.allowed_extensions.contains(&"md".to_string()));
        assert!(config.allowed_extensions.contains(&"txt".to_string()));
        assert!(config.allowed_extensions.contains(&"html".to_string()));
    }

    #[test]
    fn test_allowed_extensions_empty_by_default() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir));

        let (config, _) = merge_config(args, None).unwrap();
        assert!(config.allowed_extensions.is_empty());
    }

    #[tokio::test]
    async fn test_allowed_extensions_from_config_are_normalized() {
        let temp_dir = make_temp_dir();
        let path = make_temp_config(&format!(
            r#"
directory = "{}"
allowed_extensions = [".md", " TXT", "md"]
"#,
            temp_dir.display()
        ));

        let args = make_test_args(None);

        let file_config = load_config_file(&path).await.ok();
        let (config, _) = merge_config(args, file_config).unwrap();
        assert_eq!(
            config.allowed_extensions,
            vec!["md".to_string(), "txt".to_string()]
        );
    }

    #[test]
    fn test_normalize_extensions_trims_and_dedups() {
        let raw = vec![
            "md".to_string(),
            " txt".to_string(),
            "MD".to_string(),
            " .html ".to_string(),
        ];
        let normalized = normalize_extensions(raw);
        assert_eq!(normalized, vec!["md", "txt", "html"]);
    }

    #[test]
    fn test_normalize_extensions_strips_leading_dot_and_drops_empty() {
        let raw = vec![
            ".md".to_string(),
            "".to_string(),
            "  ".to_string(),
            "txt".to_string(),
        ];
        let normalized = normalize_extensions(raw);
        assert_eq!(normalized, vec!["md", "txt"]);
    }

    #[test]
    fn test_merge_cli_allowed_extensions_trims_whitespace() {
        // Simulates clap splitting `--allowed-extensions md, txt` into
        // ["md", " txt"] without trimming.
        let temp_dir = make_temp_dir();
        let mut args = make_test_args(Some(temp_dir));
        args.allowed_extensions = Some(vec!["md".to_string(), " txt".to_string()]);

        let (config, _) = merge_config(args, None).unwrap();
        assert_eq!(
            config.allowed_extensions,
            vec!["md".to_string(), "txt".to_string()]
        );
    }

    #[test]
    fn test_normalized_extensions_match_should_index_extension() {
        // Pins the user-visible effect: malformed entries (leading dot, leading
        // whitespace) are normalized so they actually match real file extensions.
        let temp_dir = make_temp_dir();
        let mut args = make_test_args(Some(temp_dir));
        args.allowed_extensions = Some(vec![".md".to_string(), " txt".to_string()]);

        let (config, _) = merge_config(args, None).unwrap();
        assert!(config.should_index_extension(Some(std::ffi::OsStr::new("md"))));
        assert!(config.should_index_extension(Some(std::ffi::OsStr::new("MD"))));
        assert!(config.should_index_extension(Some(std::ffi::OsStr::new("txt"))));
        assert!(!config.should_index_extension(Some(std::ffi::OsStr::new("html"))));
        assert!(!config.should_index_extension(None));
    }

    #[tokio::test]
    async fn test_merge_file_config_fallback() {
        let path = make_temp_config(
            r#"
directory = "/tmp/test"
port = 9090
bind = "0.0.0.0"
max_file_size_mb = 10
batch_size = 200
batch_timeout_ms = 5000
"#,
        );

        // CLI doesn't specify any optional fields
        let args = make_test_args(None);

        let file_config = load_config_file(&path).await.ok();
        let (config, _) = merge_config(args, file_config).unwrap();

        // All values should come from the config file
        assert_eq!(config.port, 9090);
        assert_eq!(config.bind, "0.0.0.0");
        assert_eq!(config.max_file_size_mb, 10);
        assert_eq!(config.batch_size, 200);
        assert_eq!(config.batch_timeout_ms, 5000);
    }

    #[test]
    fn test_merge_defaults_when_nothing_specified() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir));

        let (config, _) = merge_config(args, None).unwrap();

        // All values should be defaults
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.bind, DEFAULT_BIND);
        assert_eq!(config.max_file_size_mb, DEFAULT_MAX_FILE_SIZE_MB);
        assert_eq!(config.batch_size, DEFAULT_BATCH_SIZE);
        assert_eq!(config.batch_timeout_ms, DEFAULT_BATCH_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn test_validate_path_rejects_dotdot_component() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (config, _) = merge_config(args, None).unwrap();

        // ".." as a path component must be rejected
        assert!(matches!(
            config.validate_path("../Cargo.toml").await,
            PathValidateResult::OutsideDirectory
        ));
        assert!(matches!(
            config.validate_path("subdir/../../etc/passwd").await,
            PathValidateResult::OutsideDirectory
        ));
    }

    #[tokio::test]
    async fn test_validate_path_allows_dotdot_in_filename() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (config, _) = merge_config(args, None).unwrap();

        // Files with ".." in their name are legitimate
        std::fs::write(temp_dir.join("a..b.txt"), "content").unwrap();
        std::fs::write(temp_dir.join("my...file.csv"), "data").unwrap();

        assert!(matches!(
            config.validate_path("a..b.txt").await,
            PathValidateResult::Valid(_)
        ));
        assert!(matches!(
            config.validate_path("my...file.csv").await,
            PathValidateResult::Valid(_)
        ));
    }

    #[tokio::test]
    async fn test_validate_path_rejects_absolute() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (config, _) = merge_config(args, None).unwrap();

        assert!(matches!(
            config.validate_path("/etc/passwd").await,
            PathValidateResult::OutsideDirectory
        ));
    }

    #[tokio::test]
    async fn test_validate_path_not_found() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (config, _) = merge_config(args, None).unwrap();

        assert!(matches!(
            config.validate_path("nonexistent.txt").await,
            PathValidateResult::NotFound
        ));
    }

    #[tokio::test]
    async fn test_validate_path_valid_subdir() {
        let temp_dir = make_temp_dir();
        let args = make_test_args(Some(temp_dir.clone()));
        let (config, _) = merge_config(args, None).unwrap();

        std::fs::create_dir_all(temp_dir.join("sub")).unwrap();
        std::fs::write(temp_dir.join("sub").join("file.txt"), "ok").unwrap();

        assert!(matches!(
            config.validate_path("sub/file.txt").await,
            PathValidateResult::Valid(_)
        ));
    }
}
