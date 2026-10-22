use super::*;

/// Asserts every extracted heading matches the expected `(text, level)` pairs.
#[track_caller]
fn assert_headers(text: &str, expected: &[(&str, u8)]) {
    let structure = structure(text);
    assert_eq!(structure.headers.len(), expected.len());
    for (i, (header, &(name, level))) in structure.headers.iter().zip(expected).enumerate() {
        assert_eq!(header.text, name, "text of heading {i}");
        assert_eq!(header.level, level, "level of heading {i}");
    }
}

/// Asserts heading levels only; used for indentation cases where names are irrelevant.
#[track_caller]
fn assert_levels(text: &str, expected: &[u8]) {
    let structure = structure(text);
    let levels: Vec<u8> = structure.headers.iter().map(|h| h.level).collect();
    assert_eq!(levels, expected);
}

#[test]
fn test_class_top_level_is_level_1() {
    assert_headers("class MyClass:\n    pass\n", &[("MyClass", 1)]);
}

#[test]
fn test_function_top_level_is_level_2() {
    assert_headers("def my_function():\n    pass\n", &[("my_function", 2)]);
}

#[test]
fn test_method_inside_class_is_level_2() {
    assert_headers(
        "class MyClass:\n    def method(self):\n        pass\n",
        &[("MyClass", 1), ("method", 2)],
    );
}

#[test]
fn test_nested_function_is_deeper() {
    assert_headers(
        "def outer():\n    def inner():\n        pass\n    return inner\n",
        &[("outer", 2), ("inner", 3)],
    );
}

#[test]
fn test_async_def_top_level_is_level_2() {
    assert_headers("async def fetch() -> None:\n    pass\n", &[("fetch", 2)]);
}

#[test]
fn test_async_method_inside_class_is_level_2() {
    assert_headers(
        "class Api:\n    async def fetch(self):\n        pass\n",
        &[("Api", 1), ("fetch", 2)],
    );
}

#[test]
fn test_nested_def_inside_async_def_is_level_3() {
    assert_headers(
        "async def outer():\n    def inner():\n        pass\n",
        &[("outer", 2), ("inner", 3)],
    );
}

#[test]
fn test_nested_class_and_method() {
    let text = "class Outer:\n    def method(self):\n        class Inner:\n            def inner_method(self):\n                pass\n        return Inner\n";
    // Levels of Outer / method / Inner / inner_method (1 func ancestor + 2)
    assert_levels(text, &[1, 2, 3, 3]);
}

#[test]
fn test_empty_structure() {
    assert_headers("", &[]);
}

#[test]
fn test_trailing_comment_stripped_from_name() {
    assert_headers(
        "class Foo: # a comment\ndef bar(): # does stuff\n",
        &[("Foo", 1), ("bar", 2)],
    );
}

#[test]
fn test_mixed_indentation_depth() {
    // 2 spaces + 1 tab + 2 spaces = 2+4+2 = 8 → depth 2
    let text = "class Outer:\n      def method(self):\n        pass\n";
    assert_levels(text, &[1, 2]);
}

#[test]
fn test_tab_indentation_depth() {
    // 2 tabs = depth 2, nested def inside nested def → level 3
    let text = "def outer():\n\t\tdef inner():\n\t\t\tpass\n    return inner\n";
    assert_levels(text, &[2, 3]);
}
