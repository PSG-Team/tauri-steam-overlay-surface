fn main() {
    // steamworks-rs loads steam_api64.dll dynamically at runtime, so the
    // vendored redistributable (resources/) must sit next to the exe.
    // `cargo run` needs it copied into the target profile dir here.
    #[cfg(windows)]
    {
        use std::{env, fs, path::PathBuf};
        let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let dll = manifest.join("resources").join("steam_api64.dll");
        if dll.exists() {
            let out = PathBuf::from(env::var("OUT_DIR").unwrap());
            // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out → profile dir is 3 up.
            if let Some(profile_dir) = out.ancestors().nth(3) {
                let _ = fs::copy(&dll, profile_dir.join("steam_api64.dll"));
            }
        }
    }
    tauri_build::build()
}
