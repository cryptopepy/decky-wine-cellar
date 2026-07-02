use crate::wine_cask::app::WineCask;
use crate::wine_cask::generate_compatibility_tool_vdf;
use crate::wine_cask::recursive_delete_dir_entry;
use crate::PeerMap;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::path::Path;
use std::{fs, io};

const MAX_VIRTUAL_TOOL_LABEL_CHARS: usize = 64;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct VirtualToolManifest {
    pub next_virtual_tool_number: u64,
    pub tools: Vec<VirtualToolConfig>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct VirtualToolConfig {
    pub id: String,
    pub user_label: String,
    pub steam_internal_name: String,
    pub directory_name: String,
    pub current_payload_release_id: Option<String>,
}

impl VirtualToolManifest {
    fn next_tool_identity(&mut self) -> (String, String, String) {
        let next_number = self.next_virtual_tool_number.max(1);
        self.next_virtual_tool_number = next_number + 1;

        let id = format!("virtual-{}", next_number);
        let steam_internal_name = format!("WineCellarVirtual{}", next_number);

        (id, steam_internal_name.clone(), steam_internal_name)
    }
}

impl WineCask {
    pub fn load_virtual_tool_manifest(&self) -> VirtualToolManifest {
        if !self.virtual_tool_manifest_path.exists() {
            return VirtualToolManifest {
                next_virtual_tool_number: 1,
                tools: Vec::new(),
            };
        }

        match fs::read_to_string(&self.virtual_tool_manifest_path) {
            Ok(contents) => match serde_json::from_str::<VirtualToolManifest>(&contents) {
                Ok(mut manifest) => {
                    if manifest.next_virtual_tool_number == 0 {
                        manifest.next_virtual_tool_number = manifest.tools.len() as u64 + 1;
                    }
                    manifest
                }
                Err(err) => {
                    warn!("Failed to parse virtual tool manifest: {}", err);
                    VirtualToolManifest {
                        next_virtual_tool_number: 1,
                        tools: Vec::new(),
                    }
                }
            },
            Err(err) => {
                warn!("Failed to read virtual tool manifest: {}", err);
                VirtualToolManifest {
                    next_virtual_tool_number: 1,
                    tools: Vec::new(),
                }
            }
        }
    }

    pub fn save_virtual_tool_manifest(&self, manifest: &VirtualToolManifest) -> Result<(), String> {
        let manifest_parent = self
            .virtual_tool_manifest_path
            .parent()
            .ok_or_else(|| "Failed to resolve virtual tool manifest parent".to_string())?;

        create_dir_all(manifest_parent)
            .map_err(|err| format!("Failed to prepare virtual tool manifest directory: {}", err))?;

        let manifest_json = serde_json::to_string_pretty(manifest)
            .map_err(|err| format!("Failed to serialize virtual tool manifest: {}", err))?;

        let temp_manifest_path = self.virtual_tool_manifest_path.with_extension("json.tmp");
        let mut temp_manifest = File::create(&temp_manifest_path)
            .map_err(|err| format!("Failed to create virtual tool manifest temp file: {}", err))?;
        temp_manifest
            .write_all(manifest_json.as_bytes())
            .map_err(|err| format!("Failed to write virtual tool manifest temp file: {}", err))?;
        temp_manifest
            .sync_all()
            .map_err(|err| format!("Failed to flush virtual tool manifest temp file: {}", err))?;
        drop(temp_manifest);

        fs::rename(&temp_manifest_path, &self.virtual_tool_manifest_path)
            .map_err(|err| format!("Failed to persist virtual tool manifest: {}", err))
    }

    pub fn create_virtual_tool_slot(&self, user_label: String) -> Result<String, String> {
        let trimmed_label = normalize_virtual_tool_label(&user_label)?;

        let mut manifest = self.load_virtual_tool_manifest();
        let (id, steam_internal_name, directory_name) = manifest.next_tool_identity();
        let tool_dir = self
            .steam_util
            .get_steam_compatibility_tools_directory()
            .join(&directory_name);

        create_dir_all(&tool_dir)
            .map_err(|err| format!("Failed to create virtual tool directory: {}", err))?;
        if let Err(err) = generate_compatibility_tool_vdf(
            tool_dir.join("compatibilitytool.vdf"),
            &steam_internal_name,
            &trimmed_label,
        ) {
            if let Err(cleanup_err) = fs::remove_dir_all(&tool_dir) {
                error!(
                    "Failed to roll back virtual tool directory after VDF write error: {}",
                    cleanup_err
                );
            }
            return Err(format!("Failed to write virtual tool VDF: {}", err));
        }

        manifest.tools.push(VirtualToolConfig {
            id,
            user_label: trimmed_label.clone(),
            steam_internal_name,
            directory_name,
            current_payload_release_id: None,
        });

        if let Err(err) = self.save_virtual_tool_manifest(&manifest) {
            if let Err(cleanup_err) = fs::remove_dir_all(&tool_dir) {
                error!(
                    "Failed to roll back virtual tool directory after manifest save error: {}",
                    cleanup_err
                );
            }
            return Err(err);
        }

        Ok(trimmed_label)
    }

    pub fn rename_virtual_tool_slot(
        &self,
        virtual_tool_id: &str,
        user_label: String,
    ) -> Result<String, String> {
        let trimmed_label = normalize_virtual_tool_label(&user_label)?;

        let mut manifest = self.load_virtual_tool_manifest();
        let Some(config) = manifest
            .tools
            .iter_mut()
            .find(|tool| tool.id == virtual_tool_id)
        else {
            return Err("Virtual tool not found".to_string());
        };

        config.user_label = trimmed_label.clone();
        let directory_name = config.directory_name.clone();
        let steam_internal_name = config.steam_internal_name.clone();
        self.save_virtual_tool_manifest(&manifest)?;

        let tool_dir = self
            .steam_util
            .get_steam_compatibility_tools_directory()
            .join(&directory_name);
        if let Err(err) = rewrite_virtual_tool_vdf(&tool_dir, &steam_internal_name, &trimmed_label)
        {
            warn!("Failed to refresh virtual tool VDF after rename: {}", err);
        }

        Ok(trimmed_label)
    }

    pub fn update_virtual_tool_payload(
        &self,
        virtual_tool_id: &str,
        release_id: Option<String>,
    ) -> Result<(), String> {
        let mut manifest = self.load_virtual_tool_manifest();
        let Some(config) = manifest
            .tools
            .iter_mut()
            .find(|tool| tool.id == virtual_tool_id)
        else {
            return Err("Virtual tool not found".to_string());
        };

        config.current_payload_release_id = release_id;
        self.save_virtual_tool_manifest(&manifest)
    }

    pub fn remove_virtual_tool_slot(&self, virtual_tool_id: &str) -> Result<String, String> {
        let mut manifest = self.load_virtual_tool_manifest();
        let Some(position) = manifest
            .tools
            .iter()
            .position(|tool| tool.id == virtual_tool_id)
        else {
            return Err("Virtual tool not found".to_string());
        };

        let removed_tool = manifest.tools.remove(position);
        self.save_virtual_tool_manifest(&manifest)?;
        Ok(removed_tool.user_label)
    }

    pub async fn remove_virtual_tool(&self, virtual_tool_id: String, peer_map: &PeerMap) {
        let Some(virtual_tool) = self.get_virtual_tool(&virtual_tool_id).await else {
            self.broadcast_notification(peer_map, "Virtual tool not found")
                .await;
            return;
        };

        let tool_dir = self
            .steam_util
            .get_steam_compatibility_tools_directory()
            .join(&virtual_tool.directory_name);

        if let Err(err) = remove_virtual_tool_directory(
            &self.steam_util.get_steam_compatibility_tools_directory(),
            &tool_dir,
        ) {
            error!("{}", err);
            self.broadcast_notification(peer_map, &err).await;
            return;
        }

        if let Err(err) = self.remove_virtual_tool_slot(&virtual_tool_id) {
            let error_message = format!(
                "Compatibility tool directory removed but virtual tool manifest update failed: {}",
                err
            );
            error!("{}", error_message);
            self.broadcast_notification(peer_map, &error_message).await;
            return;
        }

        self.sync_backend_state().await;
        self.broadcast_app_state(peer_map).await;
        self.broadcast_notification(
            peer_map,
            &format!(
                "Removed virtual compatibility tool: {}",
                virtual_tool.user_label
            ),
        )
        .await;
    }
}

fn rewrite_virtual_tool_vdf(
    tool_dir: &std::path::Path,
    steam_internal_name: &str,
    user_label: &str,
) -> io::Result<()> {
    create_dir_all(tool_dir)?;
    generate_compatibility_tool_vdf(
        tool_dir.join("compatibilitytool.vdf"),
        steam_internal_name,
        user_label,
    )?;
    Ok(())
}

fn remove_virtual_tool_directory(base_dir: &Path, tool_dir: &Path) -> Result<(), String> {
    if !tool_dir.exists() {
        return Ok(());
    }

    let canonical_base = base_dir.canonicalize().map_err(|err| {
        format!(
            "Failed to access compatibility tools base directory: {}",
            err
        )
    })?;
    let canonical_target = tool_dir
        .canonicalize()
        .map_err(|err| format!("Failed to access virtual tool directory: {}", err))?;

    if !canonical_target.starts_with(&canonical_base) {
        return Err("Refusing to remove path outside compatibilitytools.d".to_string());
    }

    recursive_delete_dir_entry(&canonical_target)
        .map_err(|err| format!("Error removing virtual compatibility tool: {}", err))
}

pub(crate) fn normalize_virtual_tool_label(user_label: &str) -> Result<String, String> {
    let trimmed_label = user_label.trim().to_string();
    if trimmed_label.is_empty() {
        return Err("Virtual tool name cannot be empty".to_string());
    }

    if trimmed_label.chars().count() > MAX_VIRTUAL_TOOL_LABEL_CHARS {
        return Err(format!(
            "Virtual tool name must be {} characters or fewer",
            MAX_VIRTUAL_TOOL_LABEL_CHARS
        ));
    }

    if trimmed_label
        .chars()
        .any(|character| character.is_control())
    {
        return Err("Virtual tool name cannot contain control characters".to_string());
    }

    Ok(trimmed_label)
}
