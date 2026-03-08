use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
fn write_multi_size_ico(png_path: &Path, ico_path: &Path) -> Result<(), String> {
    use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
    use image::imageops::FilterType;

    let image = image::open(png_path).map_err(|err| err.to_string())?;
    let mut icon_dir = IconDir::new(ResourceType::Icon);

    for size in [16, 24, 32, 48, 64, 128, 256] {
        let resized = image
            .resize_exact(size, size, FilterType::Lanczos3)
            .into_rgba8();
        let icon = IconImage::from_rgba_data(size, size, resized.into_raw());
        let entry = IconDirEntry::encode(&icon).map_err(|err| err.to_string())?;
        icon_dir.add_entry(entry);
    }

    let mut file = File::create(ico_path).map_err(|err| err.to_string())?;
    icon_dir.write(&mut file).map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn embed_windows_icon() -> Result<(), String> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|err| err.to_string())?);
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|err| err.to_string())?);
    let png_path = manifest_dir.join("assets").join("img").join("icon.png");
    let ico_path = out_dir.join("vibealong.ico");

    write_multi_size_ico(&png_path, &ico_path)?;

    let mut resource = winres::WindowsResource::new();
    resource.set_icon(ico_path.to_string_lossy().as_ref());
    resource.compile().map_err(|err| err.to_string())?;
    Ok(())
}

fn main() {
    println!("cargo:rerun-if-changed=assets/img/icon.png");

    #[cfg(target_os = "windows")]
    if let Err(err) = embed_windows_icon() {
        panic!("Failed to embed Windows icon resource: {err}");
    }
}
