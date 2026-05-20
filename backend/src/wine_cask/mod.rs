use crate::wine_cask::app::{Command, OperationState, WineCask};
use crate::PeerMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

pub mod app;
pub mod flavors;
pub mod install;
pub mod uninstall;
pub mod virtual_tools;

pub fn generate_compatibility_tool_vdf(path: PathBuf, internal_name: &str, display_name: &str) {
    let mut file = File::create(path).expect("Failed to create file");
    writeln!(
        file,
        r#""compatibilitytools"
            {{
              "compat_tools"
              {{
                "{}"
                {{
                  "install_path" "."
                  "display_name" "{}"
                  "from_oslist"  "windows"
                  "to_oslist"    "linux"
                }}
              }}
            }}"#,
        internal_name, display_name
    )
    .expect("Failed to write to file");
}

fn recursive_delete_dir_entry(entry_path: &Path) -> io::Result<()> {
    if entry_path.is_dir() {
        for entry in fs::read_dir(entry_path)? {
            let entry = entry?;
            let path = entry.path();
            recursive_delete_dir_entry(&path)?;
        }
        fs::remove_dir(entry_path)?;
    } else {
        fs::remove_file(entry_path)?;
    }

    Ok(())
}

pub async fn process_queue(wine_cask: Arc<WineCask>, peer_map: PeerMap) {
    wine_cask.check_for_flavor_updates(&peer_map, false).await;

    loop {
        if let Some(queued_operation) = wine_cask.begin_next_operation(&peer_map).await {
            match queued_operation.command {
                Command::InstallCatalogRelease { release_id, target } => {
                    wine_cask
                        .install_catalog_release(release_id, target, &peer_map)
                        .await;
                }
                Command::UninstallInstalledTool { installed_tool_id } => {
                    wine_cask
                        .update_current_operation(OperationState::Running, 0, &peer_map)
                        .await;
                    wine_cask
                        .uninstall_installed_tool(installed_tool_id, &peer_map)
                        .await;
                }
                Command::CreateVirtualTool { user_label } => {
                    wine_cask
                        .update_current_operation(OperationState::Running, 0, &peer_map)
                        .await;
                    match wine_cask.create_virtual_tool_slot(user_label) {
                        Ok(label) => {
                            wine_cask.sync_backend_state().await;
                            wine_cask.broadcast_app_state(&peer_map).await;
                            wine_cask
                                .broadcast_notification(
                                    &peer_map,
                                    &format!("Created virtual compatibility tool: {}", label),
                                )
                                .await;
                        }
                        Err(err) => {
                            wine_cask.broadcast_notification(&peer_map, &err).await;
                        }
                    }
                }
                Command::RenameVirtualTool {
                    virtual_tool_id,
                    user_label,
                } => {
                    wine_cask
                        .update_current_operation(OperationState::Running, 0, &peer_map)
                        .await;
                    match wine_cask.rename_virtual_tool_slot(&virtual_tool_id, user_label) {
                        Ok(label) => {
                            wine_cask.sync_backend_state().await;
                            wine_cask.broadcast_app_state(&peer_map).await;
                            wine_cask
                                .broadcast_notification(
                                    &peer_map,
                                    &format!("Renamed virtual compatibility tool to {}", label),
                                )
                                .await;
                        }
                        Err(err) => {
                            wine_cask.broadcast_notification(&peer_map, &err).await;
                        }
                    }
                }
                Command::RemoveVirtualTool { virtual_tool_id } => {
                    wine_cask
                        .update_current_operation(OperationState::Running, 0, &peer_map)
                        .await;
                    wine_cask.remove_virtual_tool(virtual_tool_id, &peer_map).await;
                }
                Command::RefreshCatalog | Command::CancelOperation { .. } => {}
            }

            wine_cask.complete_current_operation(&peer_map).await;
            wine_cask.reclaim_memory_if_idle(&peer_map).await;
            continue;
        }

        wine_cask.queue_notify.notified().await;
    }
}
