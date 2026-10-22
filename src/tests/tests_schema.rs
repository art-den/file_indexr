use super::*;

#[test]
fn test_schema_fields_exist() {
    let schema = build_schema();
    for name in [
        field::PATH,
        field::PATH_EXACT,
        field::FILENAME,
        field::SIZE,
        field::MODIFIED,
        field::EXTENSION,
        field::HAS_CONTENT,
    ] {
        assert!(schema.get_field(name).is_ok(), "missing field: {name}");
    }
}
