use cargo_metadata::camino::Utf8PathBuf;
use log::info;

use crate::errors::CliError;
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(feature = "fetch-template")]
async fn fetch_template() -> reqwest::Result<Vec<u8>> {
    info!("Fetching template...");
    let response =
        reqwest::get("https://github.com/vexide/vexide-template/archive/refs/heads/main.tar.gz")
            .await?;
    let bytes = response.bytes().await?;
    info!("Successfully fetched template.");
    Ok(bytes.to_vec())
}

fn baked_in_template() -> Vec<u8> {
    include_bytes!("./vexide-template.tar.gz").to_vec()
}

fn unpack_template(template: Vec<u8>, dir: &Utf8PathBuf) -> io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&template[..]));
    for entry in archive.entries()? {
        let mut entry = entry?;

        let path = entry.path()?;
        let stripped_path = path.iter().skip(1).collect::<PathBuf>();

        if let Some(stripped_path) = stripped_path.to_str() {
            let output_path = Path::new(dir).join(stripped_path);

            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            entry.unpack(output_path)?;
        }
    }
    Ok(())
}

pub async fn new(path: Utf8PathBuf, name: Option<String>) -> Result<(), CliError> {
    let dir = if let Some(name) = &name {
        let dir = path.join(name);
        std::fs::create_dir_all(&path).unwrap();
        dir
    } else {
        path
    };

    if std::fs::read_dir(&dir).is_ok_and(|e| e.count() > 0) {
        return Err(CliError::ProjectDirFull(dir.into_string()));
    }

    let name = name.unwrap_or_else(|| dir.file_name().unwrap().to_string());
    info!("Creating new project at {:?}", dir);

    #[cfg(feature = "fetch-template")]
    let template = match fetch_template().await {
        Ok(bytes) => bytes,
        Err(_) => {
            info!("Failed to fetch template, using baked-in template.");
            baked_in_template()
        }
    };
    #[cfg(not(feature = "fetch-template"))]
    let template = baked_in_template();

    info!("Unpacking template...");
    unpack_template(template, &dir)?;
    info!("Successfully unpacked vexide-template!");

    info!("Renaming project to {}...", &name);
    let manifest_path = dir.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)?;
    let manifest = manifest.replace("vexide-template", &name);
    std::fs::write(manifest_path, manifest)?;

    info!("Successfully created new project at {:?}", dir);
    Ok(())
}
