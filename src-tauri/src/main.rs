#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod engine;
mod recording;
mod resample;
mod storage;

use engine::{Action, AppState, Engine, Preferences};
use recording::Snapshot;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
fn recording_status(state: State<'_, AppState>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
async fn recording_action(state: State<'_, AppState>, action: Action) -> Result<Snapshot, String> {
    state
        .engine()?
        .request(move |worker| worker.action(action))
        .await
}

#[tauri::command]
async fn preferences(state: State<'_, AppState>) -> Result<Preferences, String> {
    state
        .engine()?
        .request(|worker| {
            worker.require_idle()?;
            Ok(worker.preferences())
        })
        .await
}

#[tauri::command]
async fn save_preferences(
    state: State<'_, AppState>,
    microphone: Option<String>,
    output: Option<String>,
    folder: PathBuf,
) -> Result<Preferences, String> {
    state
        .engine()?
        .request(move |worker| worker.set_preferences(microphone, output, folder))
        .await
}

#[tauri::command]
async fn test_devices(
    state: State<'_, AppState>,
    microphone: Option<String>,
    output: Option<String>,
) -> Result<Snapshot, String> {
    state
        .engine()?
        .request(move |worker| worker.test_devices(microphone, output))
        .await
}

#[tauri::command]
async fn choose_folder(app: AppHandle, title: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(title)
            .blocking_pick_folder()
            .map(|path| {
                path.into_path()
                    .map(|path| storage::display_path(&path))
                    .map_err(|error| error.to_string())
            })
            .transpose()
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn list_recordings(state: State<'_, AppState>) -> Result<storage::Library, String> {
    state
        .engine()?
        .request(|worker| {
            worker.require_idle()?;
            Ok(storage::library(&worker.settings))
        })
        .await
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FileAction {
    OpenFolder,
    Play { track: usize },
    Rename { name: String },
    Export { destination: PathBuf },
    Delete,
}

#[tauri::command]
async fn recording_file_action(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    action: FileAction,
) -> Result<Option<String>, String> {
    state
        .engine()?
        .request(move |worker| {
            worker.require_idle()?;
            let (folder, session) = storage::find(&worker.settings, &id)?;
            match action {
                FileAction::OpenFolder => app
                    .opener()
                    .open_path(storage::display_path(&folder), None::<&str>)?,
                FileAction::Play { track } => {
                    let path = storage::track_path(&folder, track)?;
                    app.opener()
                        .open_path(storage::display_path(&path), None::<&str>)?;
                }
                FileAction::Rename { name } => storage::rename(&folder, session, &name)?,
                FileAction::Export { destination } => {
                    let path = storage::export(&folder, &destination, &session)?;
                    return Ok(Some(storage::display_path(&path)));
                }
                FileAction::Delete => storage::delete(&folder, &session)?,
            }
            Ok(None)
        })
        .await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let paths = app.path();
            let engine = Engine::start(
                paths.app_local_data_dir()?,
                paths.app_local_data_dir()?.join("Recordings"),
            )
            .map_err(|error| error.to_string());
            app.manage(AppState { engine });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let Ok(engine) = window.state::<AppState>().engine() else {
                    return;
                };
                api.prevent_close();
                if engine.closing.swap(true, Ordering::AcqRel) {
                    return;
                }
                let app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    if engine
                        .request(|worker| worker.stop(None, false))
                        .await
                        .is_ok()
                    {
                        app.exit(0);
                    } else {
                        engine.closing.store(false, Ordering::Release);
                    }
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            recording_status,
            recording_action,
            preferences,
            save_preferences,
            test_devices,
            choose_folder,
            list_recordings,
            recording_file_action,
        ])
        .run(tauri::generate_context!())
        .expect("NexQ Recorder could not start");
}
