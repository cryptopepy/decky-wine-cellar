use crate::wine_cask::app::WineCask;
use crate::wine_cask::generate_compatibility_tool_vdf;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use std::fs::create_dir_all;
use std::{fs, io};

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

        fs::write(&self.virtual_tool_manifest_path, manifest_json)
            .map_err(|err| format!("Failed to persist virtual tool manifest: {}", err))
    }

    pub fn create_virtual_tool_slot(&self, user_label: String) -> Result<String, String> {
        let trimmed_label = user_label.trim().to_string();
        if trimmed_label.is_empty() {
            return Err("Virtual tool name cannot be empty".to_string());
        }

        let mut manifest = self.load_virtual_tool_manifest();
        let (id, steam_internal_name, directory_name) = manifest.next_tool_identity();
        let tool_dir = self
            .steam_util
            .get_steam_compatibility_tools_directory()
            .join(&directory_name);

        create_dir_all(&tool_dir)
            .map_err(|err| format!("Failed to create virtual tool directory: {}", err))?;
        generate_compatibility_tool_vdf(
            tool_dir.join("compatibilitytool.vdf"),
            &steam_internal_name,
            &trimmed_label,
        );

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
        let trimmed_label = user_label.trim().to_string();
        if trimmed_label.is_empty() {
            return Err("Virtual tool name cannot be empty".to_string());
        }

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
    );
    Ok(())
}
