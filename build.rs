//! Explorer reads an executable's icon from a resource inside it. macOS and
//! Linux attach the same mark outside the binary — an `.icns` in the bundle,
//! PNGs beside the desktop entry — so this runs only when the binary itself
//! is what Windows will show.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=assets/windows/dbdelve.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/windows/dbdelve.ico");
        resource
            .compile()
            .unwrap_or_else(|error| panic!("could not embed the Windows icon: {error}"));
    }
}
