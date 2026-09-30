use super::*;

#[test]
fn test_schema_fields_exist() {
    let schema = build_schema();
    assert!(schema.get_field(field::PATH).is_ok());
    assert!(schema.get_field(field::PATH_EXACT).is_ok());
    assert!(schema.get_field(field::FILENAME).is_ok());
    assert!(schema.get_field(field::SIZE).is_ok());
    assert!(schema.get_field(field::MODIFIED).is_ok());
    assert!(schema.get_field(field::EXTENSION).is_ok());
    assert!(schema.get_field(field::HAS_CONTENT).is_ok());
}
