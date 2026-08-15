use std::path::PathBuf;

fn main() {
    tauri_build::build();

    // Test binaries link comctl32 v6 functions (SetWindowSubclass, TaskDialogIndirect…),
    // but without an embedded manifest the loader binds comctl32 v5.82, which lacks those
    // exports -> STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139). Embed the v6 manifest for tests.
    let manifest = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
        <assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\" manifestVersion=\"1.0\">\n\
        <dependency>\n\
        <dependentAssembly>\n\
        <assemblyIdentity type=\"win32\" name=\"Microsoft.Windows.Common-Controls\" \
        version=\"6.0.0.0\" processorArchitecture=\"*\" publicKeyToken=\"6595b64144ccf1df\" language=\"*\"/>\n\
        </dependentAssembly>\n\
        </dependency>\n\
        </assembly>\n";
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_path = out.join("test_app.manifest");
    std::fs::write(&manifest_path, manifest).expect("write test manifest");

    let arg = format!("/MANIFESTINPUT:{}", manifest_path.to_string_lossy());
    println!("cargo:rustc-link-arg-tests={arg}");
    println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
    println!("cargo:rerun-if-changed=build.rs");
}
