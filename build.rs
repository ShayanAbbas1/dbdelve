//! Explorer reads an executable's icon from a resource inside it. macOS and
//! Linux attach the same mark outside the binary — an `.icns` in the bundle,
//! PNGs beside the desktop entry — so this runs only when the binary itself
//! is what Windows will show.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DBDELVE_CHANNEL");
    println!("cargo:rerun-if-env-changed=DBDELVE_VERSION");

    #[cfg(target_os = "windows")]
    {
        let icon = match std::env::var("DBDELVE_CHANNEL").as_deref() {
            Ok("nightly") => "assets/windows/dbdelve-nightly.ico",
            _ => "assets/windows/dbdelve.ico",
        };
        println!("cargo:rerun-if-changed={icon}");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(icon);
        resource
            .compile()
            .unwrap_or_else(|error| panic!("could not embed the Windows icon: {error}"));
    }
}
