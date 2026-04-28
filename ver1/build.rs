//! Build-time tasks:
//!   1. Generate a multi-size .ico from `assets/icon.png` and embed it as the
//!      Windows resource so Explorer / taskbar / Alt-Tab show our mascot.
//!   2. Skip silently if the source PNG is missing — keeps `cargo build`
//!      working in shallow checkouts that exclude assets.

use std::fs::File;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    println!("cargo:rerun-if-changed=build.rs");

    if !cfg!(target_os = "windows") {
        return;
    }

    let src = PathBuf::from("assets/icon.png");
    if !src.exists() {
        println!("cargo:warning=assets/icon.png missing — exe will use the default icon");
        return;
    }

    let img = match image::open(&src) {
        Ok(i) => i.into_rgba8(),
        Err(e) => {
            println!("cargo:warning=failed to read icon.png: {e}");
            return;
        }
    };

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let ico_path = PathBuf::from(&out_dir).join("icon.ico");

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &size in &[16u32, 32, 48, 64, 128, 256] {
        let resized = image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3);
        let icon_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        match ico::IconDirEntry::encode(&icon_image) {
            Ok(entry) => icon_dir.add_entry(entry),
            Err(e) => {
                println!("cargo:warning=ico encode failed for size {size}: {e}");
            }
        }
    }

    match File::create(&ico_path) {
        Ok(f) => {
            if let Err(e) = icon_dir.write(f) {
                println!("cargo:warning=ico write failed: {e}");
                return;
            }
        }
        Err(e) => {
            println!("cargo:warning=ico create failed: {e}");
            return;
        }
    }

    let mut res = winres::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap());
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon embed failed: {e}");
    }
}
