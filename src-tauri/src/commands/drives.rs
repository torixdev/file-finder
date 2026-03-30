use crate::platform::list_available_drives;
use crate::types::DriveInfo;

#[tauri::command]
pub fn get_available_drives() -> Vec<DriveInfo> {
    list_available_drives()
}