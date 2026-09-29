use crate::formats::python::*;

#[test]
fn test_class_top_level_is_level_1() {
    let text = "class MyClass:\n    pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 1);
    assert_eq!(structure.headers[0].text, "MyClass");
    assert_eq!(structure.headers[0].level, 1);
}

#[test]
fn test_function_top_level_is_level_2() {
    let text = "def my_function():\n    pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 1);
    assert_eq!(structure.headers[0].text, "my_function");
    assert_eq!(structure.headers[0].level, 2);
}

#[test]
fn test_method_inside_class_is_level_2() {
    let text = "class MyClass:\n    def method(self):\n        pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "MyClass");
    assert_eq!(structure.headers[0].level, 1);
    assert_eq!(structure.headers[1].text, "method");
    assert_eq!(structure.headers[1].level, 2);
}

#[test]
fn test_nested_function_is_deeper() {
    let text = "def outer():\n    def inner():\n        pass\n    return inner\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "outer");
    assert_eq!(structure.headers[0].level, 2);
    assert_eq!(structure.headers[1].text, "inner");
    assert_eq!(structure.headers[1].level, 3);
}

#[test]
fn test_async_def_top_level_is_level_2() {
    let text = "async def fetch() -> None:\n    pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 1);
    assert_eq!(structure.headers[0].text, "fetch");
    assert_eq!(structure.headers[0].level, 2);
}

#[test]
fn test_async_method_inside_class_is_level_2() {
    let text = "class Api:\n    async def fetch(self):\n        pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "Api");
    assert_eq!(structure.headers[0].level, 1);
    assert_eq!(structure.headers[1].text, "fetch");
    assert_eq!(structure.headers[1].level, 2);
}

#[test]
fn test_nested_def_inside_async_def_is_level_3() {
    let text = "async def outer():\n    def inner():\n        pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "outer");
    assert_eq!(structure.headers[0].level, 2);
    assert_eq!(structure.headers[1].text, "inner");
    assert_eq!(structure.headers[1].level, 3);
}

#[test]
fn test_nested_class_and_method() {
    let text = "class Outer:\n    def method(self):\n        class Inner:\n            def inner_method(self):\n                pass\n        return Inner\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 4);
    assert_eq!(structure.headers[0].level, 1); // Outer
    assert_eq!(structure.headers[1].level, 2); // method
    assert_eq!(structure.headers[2].level, 3); // Inner
    assert_eq!(structure.headers[3].level, 3); // inner_method (1 func ancestor + 2)
}

#[test]
fn test_empty_structure() {
    let structure = structure("");
    assert_eq!(structure.headers.len(), 0);
}

#[test]
fn test_trailing_comment_stripped_from_name() {
    let text = "class Foo: # a comment\ndef bar(): # does stuff\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].text, "Foo");
    assert_eq!(structure.headers[1].text, "bar");
}

#[test]
fn test_mixed_indentation_depth() {
    // 2 spaces + 1 tab + 2 spaces = 2+4+2 = 8 → depth 2
    let text = "class Outer:\n      def method(self):\n        pass\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].level, 1);
    assert_eq!(structure.headers[1].level, 2);
}

#[test]
fn test_tab_indentation_depth() {
    // 2 tabs = depth 2, nested def inside nested def → level 3
    let text = "def outer():\n\t\tdef inner():\n\t\t\tpass\n    return inner\n";
    let structure = structure(text);
    assert_eq!(structure.headers.len(), 2);
    assert_eq!(structure.headers[0].level, 2); // outer
    assert_eq!(structure.headers[1].level, 3); // inner
}
