//! Gives the Windows executable its icon and version details, which
//! Explorer, the taskbar and the installer's shortcuts show.

fn main() {
    println!("cargo:rerun-if-changed=assets/gitkrill.ico");
    // Read with `option_env!` by the updater: official builds come from CI.
    println!("cargo:rerun-if-env-changed=GITKRILL_OFFICIAL_BUILD");

    // The target, not the machine running this script.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/gitkrill.ico")
        .set("ProductName", "GitKrill")
        .set("FileDescription", "GitKrill")
        .set("LegalCopyright", "MIT licence");
    if let Err(error) = resource.compile() {
        // Without the Windows SDK's resource compiler the program still
        // builds, just without its icon.
        println!("cargo:warning=no icon in the executable: {error}");
    }
}
