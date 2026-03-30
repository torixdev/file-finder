use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[tauri::command]
pub async fn open_file(path: String) -> Result<(), String> {
    use std::process::Command;
    Command::new("cmd")
        .args(["/C", "start", "", &path])
        .spawn()
        .map_err(|e| format!("Не удалось открыть файл: {}", e))?;
    Ok(())
}

#[tauri::command]
pub async fn open_folder(path: String) -> Result<(), String> {
    use std::process::Command;
    let folder_path = Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(path);
    Command::new("explorer")
        .arg(&folder_path)
        .spawn()
        .map_err(|e| format!("Не удалось открыть папку: {}", e))?;
    Ok(())
}

#[tauri::command]
pub async fn open_folder_and_select(path: String) -> Result<(), String> {
    use std::process::Command;
    Command::new("explorer")
        .args(["/select,", &path])
        .spawn()
        .map_err(|e| format!("Не удалось открыть: {}", e))?;
    Ok(())
}

#[tauri::command]
pub async fn delete_file(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if p.is_dir() {
        fs::remove_dir_all(p).map_err(|e| format!("Не удалось удалить папку: {}", e))?;
    } else {
        fs::remove_file(p).map_err(|e| format!("Не удалось удалить файл: {}", e))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn rename_file(old_path: String, new_name: String) -> Result<String, String> {
    let old = Path::new(&old_path);
    let parent = old
        .parent()
        .ok_or("Невозможно получить родительскую папку")?;
    let new_path = parent.join(&new_name);
    fs::rename(old, &new_path).map_err(|e| format!("Не удалось переименовать: {}", e))?;
    Ok(new_path.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn copy_path_to_clipboard(path: String) -> Result<(), String> {
    use std::process::Command;
    let powershell_cmd = format!("Set-Clipboard -Value '{}'", path.replace('\'', "''"));
    Command::new("powershell")
        .args(["-Command", &powershell_cmd])
        .output()
        .map_err(|e| format!("Не удалось скопировать путь: {}", e))?;
    Ok(())
}

#[tauri::command]
pub fn get_file_properties(path: String) -> Result<HashMap<String, String>, String> {
    let p = Path::new(&path);
    let meta = fs::metadata(p).map_err(|e| format!("Не удалось получить информацию: {}", e))?;

    let mut props = HashMap::new();
    props.insert("size".to_string(), meta.len().to_string());
    props.insert("is_dir".to_string(), meta.is_dir().to_string());
    props.insert("is_file".to_string(), meta.is_file().to_string());
    props.insert(
        "readonly".to_string(),
        meta.permissions().readonly().to_string(),
    );

    if let Ok(modified) = meta.modified() {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            props.insert("modified".to_string(), duration.as_secs().to_string());
        }
    }

    Ok(props)
}