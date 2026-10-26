//! Explicit bootstrap bridge; not a dependency of standalone providers.
use anyhow::Result;
pub use brick_compiler::Compiler;
pub use brick_contract::*;
use std::sync::atomic::AtomicBool;
pub fn compile_schema(schema: &str) -> Result<Vec<u8>> {
    schema_input(schema)?;
    Compiler::discover()?.compile(schema, &AtomicBool::new(false))
}
pub fn build(source: &str) -> Result<Bundle> {
    let definition = parse(source)?;
    schema_input(&definition.schema)?;
    build_with_compiler(source, &Compiler::discover()?, &AtomicBool::new(false))
}
pub fn build_with_compiler(
    source: &str,
    compiler: &Compiler,
    cancel: &AtomicBool,
) -> Result<Bundle> {
    let definition = parse(source)?;
    let descriptor = compiler.compile(&definition.schema, cancel)?;
    brick_generator::from_descriptor(source, descriptor, &compiler.identity, cancel)
}
#[cfg(test)]
mod tests {
    use super::*;
    const ECHO: &str = include_str!("../../../interfaces/echo.brick.toml");

    #[test]
    fn imports_cannot_hide_behind_comments_or_whitespace() {
        for source in [
            "import \"x.proto\";",
            "/* comment */ import public \"x\";",
            "import\n\"x\";",
        ] {
            assert!(has_import(source));
            assert!(
                compile_schema(source)
                    .unwrap_err()
                    .to_string()
                    .contains("IMPORTS_FORBIDDEN")
            );
        }
        assert!(!has_import(
            "// import x\nmessage Important { string import_name = 1; }"
        ));
        assert!(!has_import("option text = \"import x\"; /* import y */"));
    }

    #[test]
    fn generation_is_deterministic_and_publishes_without_overwrite() {
        let a = build(ECHO).unwrap();
        let b = build(ECHO).unwrap();
        assert_eq!(a.digest, b.digest);
        assert!(!a.rust_bindings.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.json");
        publish(&a, &path).unwrap();
        assert!(publish(&b, &path).is_err());
        assert_eq!(load(&path).unwrap().digest, a.digest);
        let mut altered = a;
        altered.source.push('\n');
        assert!(altered.validate().is_err());
    }

    #[test]
    fn invalid_scenarios_and_streams_are_rejected() {
        assert!(build(&ECHO.replace("hello\"}", "hello\",\"extra\":true}")).is_err());
        assert!(build(&ECHO.replace("rpc Say(Request)", "rpc Say(stream Request)")).is_err());
        assert!(build(&ECHO.replace("method = \"Say\"", "method = \"Missing\"")).is_err());
    }
}
