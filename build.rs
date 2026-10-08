fn main() {
    println!("cargo:rerun-if-changed=assets/logo.ico");

    let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/logo.ico");
    assert!(
        std::path::Path::new(icon).is_file(),
        "Icon file not found: {icon}"
    );

    let mut resource = winres::WindowsResource::new();
    resource.set_icon(icon);
    resource.compile().expect("Could not embed icon");
}
