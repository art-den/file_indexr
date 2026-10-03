use super::*;

#[test]
fn test_status_codes() {
    let cases = [
        (
            AppError::NotFound { path: "a".into() },
            StatusCode::NOT_FOUND,
        ),
        (
            AppError::OutsideDirectory { path: "a".into() },
            StatusCode::FORBIDDEN,
        ),
        (
            AppError::NotAFile { path: "a".into() },
            StatusCode::BAD_REQUEST,
        ),
        (
            AppError::UnsupportedFormat {
                extension: "foo".into(),
            },
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (AppError::too_large(10, 5), StatusCode::PAYLOAD_TOO_LARGE),
        (
            AppError::Io {
                path: "a".into(),
                source: std::io::Error::other("boom"),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            AppError::Conversion {
                path: "a".into(),
                detail: "d".into(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.status(), expected);
    }
}

#[test]
fn test_from_io_not_found_vs_generic() {
    let path = Path::new("missing.txt");

    let not_found = AppError::from_io(std::io::Error::from(std::io::ErrorKind::NotFound), path);
    assert!(matches!(not_found, AppError::NotFound { .. }));

    let other = AppError::from_io(std::io::Error::other("denied"), path);
    assert!(matches!(other, AppError::Io { .. }));
}

#[test]
fn test_from_invalid_path() {
    let outside = AppError::from_invalid_path("../x", PathValidateResult::OutsideDirectory);
    assert!(matches!(outside, AppError::OutsideDirectory { .. }));

    let missing = AppError::from_invalid_path("nope", PathValidateResult::NotFound);
    assert!(matches!(missing, AppError::NotFound { .. }));
}

#[test]
fn test_unsupported_format() {
    let with_ext = AppError::unsupported_format(Path::new("a.xyz"));
    assert_eq!(with_ext.to_string(), "Unsupported format: xyz");

    let no_ext = AppError::unsupported_format(Path::new("README"));
    assert_eq!(no_ext.to_string(), "Unsupported format: (no extension)");
}

#[test]
fn test_display_messages() {
    assert_eq!(
        AppError::NotFound {
            path: "a.txt".into()
        }
        .to_string(),
        "file not found: a.txt"
    );
    assert_eq!(
        AppError::OutsideDirectory {
            path: "../x".into()
        }
        .to_string(),
        "path is outside the watched directory: ../x"
    );
    assert_eq!(
        AppError::NotAFile { path: "d".into() }.to_string(),
        "not a regular file: d"
    );
    assert_eq!(
        AppError::too_large(10, 5).to_string(),
        "file is too large: 10 bytes (limit 5 bytes)"
    );
    assert_eq!(
        AppError::Io {
            path: "a.txt".into(),
            source: std::io::Error::other("boom"),
        }
        .to_string(),
        "failed to read a.txt: boom"
    );
}
