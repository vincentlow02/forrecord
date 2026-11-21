#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod engine;
mod playback;
mod recording;
mod resample;
mod storage;

use engine::{Action, AppState, Engine, Preferences};
use playback::Player;
use recording::Snapshot;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
fn recording_status(state: State<'_, AppState>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
async fn recording_action(
    state: State<'_, AppState>,
    player: State<'_, Player>,
    action: Action,
) -> Result<Snapshot, String> {
    if matches!(action, Action::Start) {
        player.release(None);
    }
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
    folder: Option<String>,
) -> Result<Preferences, String> {
    let folder = folder
        .map(|selection| {
            let mut grant = state.folder_grant.lock().unwrap();
            match grant.as_ref() {
                Some(path) if storage::display_path(path) == selection => Ok(grant.take().unwrap()),
                _ => Err("Use Browse to select a recording folder."),
            }
        })
        .transpose()?;
    state
        .engine()?
        .request(move |worker| {
            let folder = folder.unwrap_or_else(|| worker.settings.folder.clone());
            worker.set_preferences(microphone, output, folder)
        })
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
async fn choose_folder(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    let folder: Option<PathBuf> = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Choose a recordings folder")
            .blocking_pick_folder()
            .map(|path| path.into_path().map_err(|error| error.to_string()))
            .transpose()
    })
    .await
    .map_err(|error| error.to_string())??;
    let display = folder.as_deref().map(storage::display_path);
    if folder.is_some() {
        *state.folder_grant.lock().unwrap() = folder;
    }
    Ok(display)
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
    Rename { name: String },
    Delete,
}

#[tauri::command]
async fn recording_file_action(
    app: AppHandle,
    state: State<'_, AppState>,
    player: State<'_, Player>,
    id: String,
    action: FileAction,
) -> Result<(), String> {
    if matches!(action, FileAction::Delete) {
        player.release(None);
    }
    state
        .engine()?
        .request(move |worker| {
            worker.require_idle()?;
            let (folder, session) = storage::find(&worker.settings, &id)?;
            match action {
                FileAction::OpenFolder => app
                    .opener()
                    .open_path(storage::display_path(&folder), None::<&str>)?,
                FileAction::Rename { name } => storage::rename(&folder, &name)?,
                FileAction::Delete => storage::delete(&folder, &session)?,
            }
            Ok(())
        })
        .await
}

#[tauri::command]
async fn export_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    track: Option<usize>,
) -> Result<Option<String>, String> {
    let engine = state.engine()?;
    let selected = id.clone();
    let name = engine
        .request(move |worker| {
            worker.require_idle()?;
            let (folder, session) = storage::find(&worker.settings, &selected)?;
            if let Some(track) = track {
                storage::track_path(&folder, track)?;
            }
            Ok(storage::suggested_name(&session, track))
        })
        .await?;
    let destination = tauri::async_runtime::spawn_blocking(move || {
        let file = app.dialog().file();
        match track {
            Some(_) => file
                .set_title("Export WAV")
                .set_file_name(&name)
                .add_filter("WAV audio", &["wav"])
                .blocking_save_file(),
            None => file
                .set_title("Choose a folder for this session")
                .blocking_pick_folder(),
        }
        .map(|path| path.into_path().map_err(|error| error.to_string()))
        .transpose()
    })
    .await
    .map_err(|error| error.to_string())??;
    let Some(destination) = destination else {
        return Ok(None);
    };
    engine
        .request(move |worker| {
            worker.require_idle()?;
            let (folder, session) = storage::find(&worker.settings, &id)?;
            let path = match track {
                Some(track) => storage::export_track(&folder, track, &destination)?,
                None => storage::export(&folder, &destination, &session)?,
            };
            Ok(Some(storage::display_path(&path)))
        })
        .await
}

#[tauri::command]
async fn prepare_playback(
    state: State<'_, AppState>,
    player: State<'_, Player>,
    id: String,
    track: usize,
) -> Result<String, String> {
    let player = player.inner().clone();
    state
        .engine()?
        .request(move |worker| {
            worker.require_idle()?;
            let (folder, _) = storage::find(&worker.settings, &id)?;
            let ticket = player.prepare(&storage::track_path(&folder, track)?)?;
            Ok(format!("http://nexq-audio.localhost/{ticket}"))
        })
        .await
}

#[tauri::command]
fn release_playback(player: State<'_, Player>, ticket: String) {
    player.release(Some(&ticket));
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Player::default())
        .register_asynchronous_uri_scheme_protocol("nexq-audio", |context, request, response| {
            let player = context.app_handle().state::<Player>().inner().clone();
            let label = context.webview_label().to_owned();
            tauri::async_runtime::spawn_blocking(move || {
                response.respond(player.serve(&label, request))
            });
        })
        .setup(|app| {
            let paths = app.path();
            let engine = Engine::start(
                paths.app_local_data_dir()?,
                paths.app_local_data_dir()?.join("Recordings"),
            )
            .map_err(|error| error.to_string());
            app.manage(AppState {
                engine,
                folder_grant: Mutex::default(),
            });
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
            export_recording,
            prepare_playback,
            release_playback,
        ])
        .run(tauri::generate_context!())
        .expect("NexQ Recorder could not start");
}
