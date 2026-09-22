//! Explorer reads an executable's icon from a resource inside it. macOS and
//! Linux attach the same mark outside the binary — an `.icns` in the bundle,
//! PNGs beside the desktop entry — so this runs only when the binary itself
//! is what Windows will show.
//!
//! The PNGs are the ones `dev/icon.swift` already produces for the Linux
//! package. They are packed into an icon here rather than checked in a second
//! time as an `.ico`.

#[path = "build/ico.rs"]
mod ico;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/ico.rs");

    #[cfg(target_os = "windows")]
    embed_icon();
}

#[cfg(target_os = "windows")]
fn embed_icon() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let sizes = [16u32, 32, 64, 128, 256];
    let mut loaded = Vec::with_capacity(sizes.len());
    for size in sizes {
        let path = manifest.join(format!("assets/linux/icons/dbdelve-{size}.png"));
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
        loaded.push((size, bytes));
    }
    let images = loaded
        .iter()
        .map(|(size, bytes)| (*size, bytes.as_slice()))
        .collect::<Vec<_>>();

    let ico_path = out_dir.join("dbdelve.ico");
    let mut file = std::fs::File::create(&ico_path)
        .unwrap_or_else(|error| panic!("could not create {}: {error}", ico_path.display()));
    ico::write_ico(&mut file, &images)
        .unwrap_or_else(|error| panic!("could not write {}: {error}", ico_path.display()));
    drop(file);

    let icon = ico_path
        .to_str()
        .unwrap_or_else(|| panic!("{} is not utf-8", ico_path.display()));
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon);
    resource
        .compile()
        .unwrap_or_else(|error| panic!("could not embed the Windows icon: {error}"));
}
