fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("Cargo should set CARGO_CFG_TARGET_OS");
        
    match target_os.as_str() {
        "windows" => {
            println!("cargo:rerun-if-changed=assets/logo.ico");

            let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/logo.ico");
            assert!(
                std::path::Path::new(icon).is_file(),
                "Icon file not found: {icon}"
            );

            let mut resource = winres::WindowsResource::new();
            resource.set_icon(icon);
            resource.compile().expect("Could not embed icon");
        }, 
        &_ => {}
    }
}
