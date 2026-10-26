fn main() {
    brick_build::contract(
        "../../interfaces/compiler.brick.toml",
        "../../fixtures/compiler.bundle.json",
    )
    .expect("standalone compiler contract");
}
