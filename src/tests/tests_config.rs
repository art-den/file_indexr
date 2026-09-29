use std::path::Path;
use std::path::PathBuf;

use crate::config::*;
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

#[test]
fn test_default_indexes_only_recognized_formats() {
    let temp_dir = make_temp_dir();
    let args = make_test_args(Some(temp_dir));

    let (config, _) = merge_config(args, None).unwrap();
    assert!(config.should_index(Path::new("notes.txt")));
    assert!(config.should_index(Path::new("doc.PDF")));
    assert!(config.should_index(Path::new("src/main.rs")));
    assert!(!config.should_index(Path::new("data.bin")));
    assert!(!config.should_index(Path::new("lib.so")));
    assert!(!config.should_index(Path::new("noextension")));
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
fn test_normalized_extensions_match_should_index() {
    // Pins the user-visible effect: malformed entries (leading dot, leading
    // whitespace) are normalized so they actually match real file extensions.
    let temp_dir = make_temp_dir();
    let mut args = make_test_args(Some(temp_dir));
    args.allowed_extensions = Some(vec![".md".to_string(), " txt".to_string()]);

    let (config, _) = merge_config(args, None).unwrap();
    assert!(config.should_index(Path::new("a.md")));
    assert!(config.should_index(Path::new("a.MD")));
    assert!(config.should_index(Path::new("b.txt")));
    assert!(!config.should_index(Path::new("c.html")));
    assert!(!config.should_index(Path::new("noext")));
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
