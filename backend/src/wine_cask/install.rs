use crate::github_util::{Asset, Release};
use crate::wine_cask::app::{InstallTarget, OperationState, WineCask};
use crate::wine_cask::flavors::{CatalogRelease, CompatibilityToolFlavor};
use crate::wine_cask::{generate_compatibility_tool_vdf, recursive_delete_dir_entry};
use crate::PeerMap;
use flate2::bufread::GzDecoder;
use futures_util::StreamExt;
use log::{error, info, warn};
use std::fs::{create_dir_all, File};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use tokio::fs::File as TokioFile;
use tokio::io::AsyncWriteExt;
use xz2::bufread::XzDecoder;

#[derive(Clone)]
struct InstallPlan {
    catalog_release: CatalogRelease,
    target: InstallTarget,
}

#[derive(Clone, Debug, PartialEq)]
enum CompressionType {
    Gzip,
    Xz,
    Unknown,
}

struct DownloadPlan {
    url: String,
    compression_type: CompressionType,
}

impl WineCask {
    pub async fn install_catalog_release(
        &self,
        release_id: String,
        target: InstallTarget,
        peer_map: &PeerMap,
    ) {
        let Some(catalog_release) = self.get_catalog_release(&release_id).await else {
            self.broadcast_notification(peer_map, "Requested release is no longer available")
                .await;
            return;
        };

        let install_plan = InstallPlan {
            catalog_release,
            target: target.clone(),
        };

        let Some(download_plan) =
            look_for_compressed_archive(&install_plan.catalog_release.release)
        else {
            self.broadcast_notification(
                peer_map,
                "Error: No supported compressed archive found for this release",
            )
            .await;
            return;
        };

        self.update_current_operation(OperationState::Downloading, 0, peer_map)
            .await;

        let temp_dir = match prepare_temp_directory(
            &self
                .steam_util
                .get_steam_compatibility_tools_directory()
                .join(".wine-cellar-staging"),
        ) {
            Some(temp_dir) => temp_dir,
            None => {
                self.broadcast_notification(
                    peer_map,
                    "Failed to prepare temporary install directory",
                )
                .await;
                return;
            }
        };
        let archive_path = temp_dir.join(download_archive_name(&download_plan.compression_type));
        let mut archive_file = match TokioFile::create(&archive_path).await {
            Ok(file) => file,
            Err(err) => {
                cleanup_temp_directory(&temp_dir);
                self.broadcast_notification(
                    peer_map,
                    &format!("Failed to create temporary archive file: {}", err),
                )
                .await;
                return;
            }
        };

        let client = reqwest::Client::new();
        let response = match client.get(&download_plan.url).send().await {
            Ok(resp) => resp,
            Err(err) => {
                error!("Download request failed: {}", err);
                cleanup_temp_directory(&temp_dir);
                self.broadcast_notification(
                    peer_map,
                    "Connection error: Unable to start compatibility tool download",
                )
                .await;
                return;
            }
        };

        if !response.status().is_success() {
            error!("Download failed with status {}", response.status());
            cleanup_temp_directory(&temp_dir);
            self.broadcast_notification(peer_map, "Connection error: Download failed")
                .await;
            return;
        }

        let total_size = response.content_length();
        let mut downloaded_size = 0u64;
        let mut body = response.bytes_stream();

        while let Some(chunk_result) = body.next().await {
            if self.current_operation_is_cancelling().await {
                cleanup_temp_directory(&temp_dir);
                self.broadcast_notification(peer_map, "Installation cancelled")
                    .await;
                return;
            }

            let chunk = match chunk_result {
                Ok(chunk) => chunk,
                Err(err) => {
                    error!("Download stream failed: {}", err);
                    cleanup_temp_directory(&temp_dir);
                    self.broadcast_notification(peer_map, "Connection error: Download interrupted")
                        .await;
                    return;
                }
            };

            if let Err(err) = archive_file.write_all(&chunk).await {
                error!("Failed to write temporary archive: {}", err);
                cleanup_temp_directory(&temp_dir);
                self.broadcast_notification(
                    peer_map,
                    "Storage error: Failed to write compatibility tool archive",
                )
                .await;
                return;
            }
            downloaded_size += chunk.len() as u64;

            if let Some(total_size) = total_size {
                if total_size > 0 {
                    let progress = ((downloaded_size as f64 / total_size as f64) * 100.0)
                        .round()
                        .clamp(0.0, 100.0) as u8;
                    self.update_current_operation(OperationState::Downloading, progress, peer_map)
                        .await;
                }
            }
        }

        if let Err(err) = archive_file.flush().await {
            error!("Failed to flush temporary archive: {}", err);
            cleanup_temp_directory(&temp_dir);
            self.broadcast_notification(
                peer_map,
                "Storage error: Failed to finalize compatibility tool archive",
            )
            .await;
            return;
        }

        drop(archive_file);

        match self
            .extract_and_install(
                peer_map,
                &install_plan,
                download_plan.compression_type,
                &temp_dir,
                &archive_path,
            )
            .await
        {
            Ok(message) => {
                info!("{}", message);
                self.sync_backend_state().await;
                self.broadcast_app_state(peer_map).await;
                self.broadcast_notification(peer_map, &message).await;
            }
            Err(err) => {
                error!("Installation failed: {}", err);
                self.broadcast_notification(peer_map, &err).await;
            }
        }

        cleanup_temp_directory(&temp_dir);
    }

    async fn extract_and_install(
        &self,
        peer_map: &PeerMap,
        install_plan: &InstallPlan,
        compression_type: CompressionType,
        temp_dir: &Path,
        archive_path: &Path,
    ) -> Result<String, String> {
        if self.current_operation_is_cancelling().await {
            return Err("Installation cancelled".to_string());
        }

        self.update_current_operation(OperationState::Extracting, 0, peer_map)
            .await;

        let temp_dir_clone = temp_dir.to_path_buf();
        let archive_path_clone = archive_path.to_path_buf();
        let unpack_result = tokio::task::spawn_blocking(move || {
            let archive_file = File::open(&archive_path_clone)
                .map_err(|err| format!("Failed to open temporary archive: {}", err))?;
            let archive_reader = BufReader::new(archive_file);

            let decompressed: Box<dyn Read> = if compression_type == CompressionType::Gzip {
                Box::new(GzDecoder::new(archive_reader))
            } else if compression_type == CompressionType::Xz {
                Box::new(XzDecoder::new(archive_reader))
            } else {
                return Err("Unsupported archive compression type".to_string());
            };

            safe_unpack_tar(decompressed, &temp_dir_clone)
        })
        .await
        .map_err(|err| format!("Extraction task failed: {}", err))?;

        if let Err(err) = unpack_result {
            return Err(format!("Installation failed: {}", err));
        }

        if let Err(err) = std::fs::remove_file(archive_path) {
            warn!(
                "Failed to delete temporary archive after extraction: {}",
                err
            );
        }

        if self.current_operation_is_cancelling().await {
            return Err("Installation cancelled".to_string());
        }

        let extracted_directory = std::fs::read_dir(temp_dir)
            .map_err(|err| format!("Failed to read extraction directory: {}", err))?
            .filter_map(Result::ok)
            .filter(|entry| entry.metadata().map(|meta| meta.is_dir()).unwrap_or(false))
            .map(|entry| entry.path())
            .find(|path| path.join("compatibilitytool.vdf").exists())
            .ok_or_else(|| {
                "Failed to find the extracted compatibility tool contents".to_string()
            })?;

        if self.current_operation_is_cancelling().await {
            return Err("Installation cancelled".to_string());
        }

        match &install_plan.target {
            InstallTarget::Direct => self.install_direct_tool(&extracted_directory, install_plan),
            InstallTarget::VirtualTool { virtual_tool_id } => {
                self.install_virtual_tool(&extracted_directory, install_plan, virtual_tool_id)
            }
        }
    }

    fn install_direct_tool(
        &self,
        extracted_directory: &Path,
        install_plan: &InstallPlan,
    ) -> Result<String, String> {
        let temp_dir = extracted_directory
            .parent()
            .ok_or_else(|| "Missing temporary extraction parent directory".to_string())?;
        let compatibility_tools_directory =
            self.steam_util.get_steam_compatibility_tools_directory();

        let new_path = match install_plan.catalog_release.flavor {
            CompatibilityToolFlavor::ProtonGE | CompatibilityToolFlavor::ProtonCachyOS => {
                extracted_directory.to_path_buf()
            }
            CompatibilityToolFlavor::SteamTinkerLaunch
            | CompatibilityToolFlavor::Luxtorpeda
            | CompatibilityToolFlavor::Boxtron => {
                let new_folder_name = format!(
                    "{}{}",
                    install_plan.catalog_release.flavor,
                    install_plan.catalog_release.release.tag_name
                );
                generate_compatibility_tool_vdf(
                    extracted_directory.join("compatibilitytool.vdf"),
                    &new_folder_name,
                    &format!(
                        "{} {}",
                        install_plan.catalog_release.flavor,
                        install_plan.catalog_release.release.tag_name
                    ),
                );
                temp_dir.join(&new_folder_name)
            }
            CompatibilityToolFlavor::Unknown => {
                return Err("Unsupported compatibility tool flavor".to_string())
            }
        };

        if new_path != extracted_directory {
            std::fs::rename(extracted_directory, &new_path)
                .map_err(|err| format!("Failed to prepare compatibility tool layout: {}", err))?;
        }

        let target_directory_name = new_path
            .file_name()
            .ok_or_else(|| "Failed to resolve extracted compatibility tool name".to_string())?;
        let target_directory = compatibility_tools_directory.join(target_directory_name);

        if target_directory.exists() {
            return Err(format!(
                "Compatibility tool directory already exists: {}",
                target_directory.display()
            ));
        }

        std::fs::rename(&new_path, &target_directory).map_err(|err| {
            format!(
                "Failed to move compatibility tool into Steam compatibilitytools.d: {}",
                err
            )
        })?;

        Ok(format!(
            "Installation completed: {}",
            install_plan.catalog_release.release.tag_name
        ))
    }

    fn install_virtual_tool(
        &self,
        extracted_directory: &Path,
        install_plan: &InstallPlan,
        virtual_tool_id: &str,
    ) -> Result<String, String> {
        let manifest = self.load_virtual_tool_manifest();
        let Some(virtual_tool) = manifest
            .tools
            .iter()
            .find(|tool| tool.id == virtual_tool_id)
        else {
            return Err("Virtual compatibility tool no longer exists".to_string());
        };

        let target_directory = self
            .steam_util
            .get_steam_compatibility_tools_directory()
            .join(&virtual_tool.directory_name);

        if target_directory.exists() {
            recursive_delete_dir_entry(&target_directory)
                .map_err(|err| format!("Failed to replace virtual tool contents: {}", err))?;
        }

        std::fs::rename(extracted_directory, &target_directory)
            .map_err(|err| format!("Failed to move virtual tool contents into place: {}", err))?;
        generate_compatibility_tool_vdf(
            target_directory.join("compatibilitytool.vdf"),
            &virtual_tool.steam_internal_name,
            &virtual_tool.user_label,
        );

        self.update_virtual_tool_payload(
            virtual_tool_id,
            Some(install_plan.catalog_release.id.clone()),
        )?;

        Ok(format!(
            "Mounted {} into {}",
            install_plan.catalog_release.release.tag_name, virtual_tool.user_label
        ))
    }
}

fn safe_unpack_tar(reader: Box<dyn Read>, destination: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);

    let entries = archive
        .entries()
        .map_err(|err| format!("Failed to read tar entries: {}", err))?;

    for entry in entries {
        let mut entry = entry.map_err(|err| format!("Failed to process tar entry: {}", err))?;
        let path = entry
            .path()
            .map_err(|err| format!("Failed to resolve tar entry path: {}", err))?;

        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(format!(
                "Unsafe archive entry path detected: {}",
                path.display()
            ));
        }

        entry
            .unpack_in(destination)
            .map_err(|err| format!("Failed to unpack archive entry: {}", err))?;
    }

    Ok(())
}

fn prepare_temp_directory(staging_root: &Path) -> Option<PathBuf> {
    let temp_dir = staging_root.join("temp");

    if temp_dir.exists() {
        warn!("Found existing temp directory, cleaning up...");
        cleanup_temp_directory(&temp_dir);
    }

    if let Err(err) = create_dir_all(&temp_dir) {
        error!("Failed to create temp directory: {}", err);
        return None;
    }

    Some(temp_dir)
}

fn download_archive_name(compression_type: &CompressionType) -> &'static str {
    match compression_type {
        CompressionType::Gzip => "download.tar.gz",
        CompressionType::Xz => "download.tar.xz",
        CompressionType::Unknown => "download.tar",
    }
}

fn cleanup_temp_directory(temp_dir: &Path) {
    if let Err(err) = recursive_delete_dir_entry(temp_dir) {
        error!("Failed to clean up temp directory: {}", err);
    }
}

fn look_for_compressed_archive(release: &Release) -> Option<DownloadPlan> {
    let is_supported_archive = |asset: &Asset| {
        let name = asset.name.to_ascii_lowercase();
        asset.content_type == "application/gzip"
            || asset.content_type == "application/x-xz"
            || name.ends_with(".tar.gz")
            || name.ends_with(".tar.xz")
    };

    let compression_type = |asset: &Asset| {
        let name = asset.name.to_ascii_lowercase();
        if name.ends_with(".tar.gz") {
            CompressionType::Gzip
        } else if name.ends_with(".tar.xz") {
            CompressionType::Xz
        } else if asset.content_type == "application/gzip" {
            CompressionType::Gzip
        } else if asset.content_type == "application/x-xz" {
            CompressionType::Xz
        } else {
            CompressionType::Unknown
        }
    };

    release
        .assets
        .iter()
        .filter(|asset| is_supported_archive(asset))
        .filter(|asset| is_steam_deck_archive(asset))
        .map(|asset| DownloadPlan {
            url: asset.browser_download_url.clone(),
            compression_type: compression_type(asset),
        })
        .next()
}

fn is_steam_deck_archive(asset: &Asset) -> bool {
    let name = asset.name.to_ascii_lowercase();
    ![
        "aarch64", "arm64", "armv7", "armhf", "riscv64", "ppc64", "s390x", "loong64",
    ]
    .iter()
    .any(|arch| name.contains(arch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_selection_skips_aarch64_build_when_generic_build_exists() {
        let release = release_with_assets(vec![
            asset(
                "GE-Proton11-1-aarch64.tar.gz",
                "https://example.com/aarch64",
            ),
            asset("GE-Proton11-1.tar.gz", "https://example.com/x86_64"),
        ]);

        let plan = look_for_compressed_archive(&release).expect("Expected x86-compatible archive");

        assert_eq!(plan.url, "https://example.com/x86_64");
        assert_eq!(plan.compression_type, CompressionType::Gzip);
    }

    #[test]
    fn archive_selection_rejects_release_with_only_aarch64_build() {
        let release = release_with_assets(vec![asset(
            "GE-Proton11-1-aarch64.tar.gz",
            "https://example.com/aarch64",
        )]);

        assert!(look_for_compressed_archive(&release).is_none());
    }

    #[test]
    fn archive_selection_accepts_explicit_x86_64_build() {
        let release = release_with_assets(vec![asset(
            "proton-cachyos-10-x86_64_v3.tar.xz",
            "https://example.com/x86_64_v3",
        )]);

        let plan = look_for_compressed_archive(&release).expect("Expected x86_64 archive");

        assert_eq!(plan.url, "https://example.com/x86_64_v3");
        assert_eq!(plan.compression_type, CompressionType::Xz);
    }

    fn release_with_assets(assets: Vec<Asset>) -> Release {
        Release {
            url: "https://example.com/release".to_string(),
            id: 1,
            draft: false,
            prerelease: false,
            name: "Release".to_string(),
            tag_name: "Release".to_string(),
            assets,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            published_at: "2026-01-01T00:00:00Z".to_string(),
            tarball_url: "https://example.com/source.tar.gz".to_string(),
            body: String::new(),
        }
    }

    fn asset(name: &str, url: &str) -> Asset {
        Asset {
            url: format!("{}/api", url),
            id: 1,
            name: name.to_string(),
            content_type: "application/gzip".to_string(),
            state: "uploaded".to_string(),
            size: 1024,
            download_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            browser_download_url: url.to_string(),
        }
    }
}
