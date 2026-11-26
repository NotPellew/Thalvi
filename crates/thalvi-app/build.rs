fn main() {
    println!("cargo:rerun-if-env-changed=THALVI_RELEASE_BUILD");
    assert!(
        !cfg!(feature = "update-fixture") || std::env::var_os("THALVI_RELEASE_BUILD").is_none(),
        "Release packages must not enable update-fixture"
    );
    tauri_build::build();
}
