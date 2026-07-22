use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    let config = cbindgen::Config {
        language: cbindgen::Language::C,
        include_guard: Some("SLATE_H".to_string()),
        sys_includes: vec!["stdint.h".into(), "stddef.h".into()],
        ..Default::default()
    };

    cbindgen::Builder::new()
        .with_crate(crate_dir.clone())
        .with_config(config)
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file("include/slate.h");
}
