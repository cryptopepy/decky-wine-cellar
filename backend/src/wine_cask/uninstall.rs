use crate::wine_cask::app::WineCask;
use crate::wine_cask::flavors::InstalledToolSource;
use crate::wine_cask::recursive_delete_dir_entry;
use crate::PeerMap;
use log::error;
use std::path::PathBuf;

impl WineCask {
    pub async fn uninstall_installed_tool(&self, installed_tool_id: String, peer_map: &PeerMap) {
        let Some(installed_tool) = self.get_installed_tool(&installed_tool_id).await else {
            self.broadcast_notification(peer_map, "Installed tool not found")
                .await;
            return;
        };

        let directory_path = PathBuf::from(&installed_tool.path);
        let base_dir = self.steam_util.get_steam_compatibility_tools_directory();

        let canonical_base = match base_dir.canonicalize() {
            Ok(base) => base,
            Err(err) => {
                let error_message = format!(
                    "Failed to access compatibility tools base directory: {}",
                    err
                );
                error!("{}", error_message);
                self.broadcast_notification(peer_map, &error_message).await;
                return;
            }
        };

        let canonical_target = match directory_path.canonicalize() {
            Ok(path) => path,
            Err(err) => {
                let error_message = format!("Failed to access uninstall path: {}", err);
                error!("{}", error_message);
                self.broadcast_notification(peer_map, &error_message).await;
                return;
            }
        };

        if !canonical_target.starts_with(&canonical_base) {
            let error_message =
                "Refusing to uninstall path outside compatibilitytools.d".to_string();
            error!("{}", error_message);
            self.broadcast_notification(peer_map, &error_message).await;
            return;
        }

        if let Err(err) = recursive_delete_dir_entry(&canonical_target) {
            let error_message = format!("Error during uninstallation: {}", err);
            error!("{}", error_message);
            self.broadcast_notification(peer_map, &error_message).await;
            return;
        }

        if matches!(installed_tool.source, InstalledToolSource::Virtual) {
            if let Some(virtual_tool_id) = &installed_tool.virtual_tool_id {
                if let Err(err) = self.remove_virtual_tool_slot(virtual_tool_id) {
                    let error_message = format!(
                        "Compatibility tool directory removed but virtual tool manifest update failed: {}",
                        err
                    );
                    error!("{}", error_message);
                    self.broadcast_notification(peer_map, &error_message).await;
                    return;
                }
            }
        }

        self.sync_backend_state().await;
        self.broadcast_app_state(peer_map).await;

        let label = installed_tool
            .user_label
            .unwrap_or(installed_tool.display_name);
        self.broadcast_notification(peer_map, &format!("Removed {}", label))
            .await;
    }
}
