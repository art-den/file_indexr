use clap::Parser;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "tests/tests_config.rs"]
mod tests;

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
    /// Allowed extensions for indexing. If empty, only files with a
    /// recognized format (`crate::formats::file_format_by_file_name`) are indexed.
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

    /// Comma-separated list of allowed extensions (e.g., "md,txt,html").
    /// If unspecified, only files with a recognized format are indexed.
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

    /// Returns `true` if the file at `path` should be indexed.
    /// If `allowed_extensions` is set, only files with one of those extensions
    /// pass; otherwise the decision is made by
    /// [`crate::formats::file_format_by_file_name`] — only files with a
    /// recognized format are indexed.
    pub fn should_index(&self, path: &Path) -> bool {
        if self.allowed_extensions.is_empty() {
            return crate::formats::file_format_by_file_name(path).is_some();
        }
        let Some(ext) = path.extension() else {
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
        // An empty path would canonicalize to the watched directory itself,
        // which is never a file.
        if rel_path.is_empty() {
            return PathValidateResult::NotFound;
        }
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
