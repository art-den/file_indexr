use tantivy::schema::{DateOptions, FAST, INDEXED, STORED, STRING, Schema, SchemaBuilder, TEXT};

/// Field names in the Tantivy schema.
pub mod field {
    pub const PATH: &str = "path";
    pub const PATH_EXACT: &str = "path_exact";
    pub const FILENAME: &str = "filename";
    pub const DIR: &str = "dir";
    pub const CONTENT: &str = "content";
    pub const SIZE: &str = "size";
    pub const MODIFIED: &str = "modified";
    pub const EXTENSION: &str = "extension";
    pub const HAS_CONTENT: &str = "has_content";
}

/// Build and return the Tantivy schema used for indexing.
pub fn build_schema() -> Schema {
    let mut schema_builder = SchemaBuilder::new();

    schema_builder.add_text_field(field::PATH, TEXT | STORED);
    schema_builder.add_text_field(field::PATH_EXACT, STRING | STORED | FAST);
    schema_builder.add_text_field(field::FILENAME, TEXT | STORED);
    schema_builder.add_text_field(field::DIR, TEXT | STORED);
    // Content field: indexed with tokenization, NOT stored
    schema_builder.add_text_field(field::CONTENT, TEXT);
    schema_builder.add_u64_field(field::SIZE, INDEXED | STORED | FAST);
    schema_builder.add_date_field(
        field::MODIFIED,
        DateOptions::default().set_indexed().set_fast(),
    );
    schema_builder.add_text_field(field::EXTENSION, STRING | STORED);
    schema_builder.add_bool_field(field::HAS_CONTENT, STORED);

    schema_builder.build()
}

#[cfg(test)]
mod tests {
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
}
