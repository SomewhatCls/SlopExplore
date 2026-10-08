fn main() {
    let target_os =
        std::env::var("CARGO_CFG_TARGET_OS").expect("Cargo should set CARGO_CFG_TARGET_OS");
    let host_os = std::env::var("HOST").expect("Cargo should set HOST");

    match (target_os.as_str(), host_os.as_str()) {
        ("windows", "windows") => {
            println!("cargo:rerun-if-changed=assets/logo.ico");

            let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/logo.ico");
            assert!(
                std::path::Path::new(icon).is_file(),
                "Icon file not found: {icon}"
            );

            let mut resource = winres::WindowsResource::new();
            resource.set_icon(icon);
            resource.compile().expect("Could not embed icon");
            return;
        }
        (&_, _) => {}
    }
    if host_os.contains("linux") && target_os.ends_with("windows-gnu") {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let icon = std::path::Path::new(&manifest_dir).join("assets/logo.ico");

        let mut resource = winres::WindowsResource::new();
        resource.set_icon(icon.to_str().unwrap());

        resource
            .set_toolkit_path("/usr/bin")
            .set_windres_path("x86_64-w64-mingw32-windres");
        resource.compile().expect("Could not embed icon");
    }
}
