// objc 0.2's msg_send! checks a `cargo-clippy` cfg that newer compilers flag.
#![cfg_attr(target_os = "macos", allow(unexpected_cfgs))]

#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

mod destinations;

use anyhow::{anyhow, Context, Result};
use arboard::Clipboard;
use base64::Engine as _;
use destinations::Destination;
use image::{ImageBuffer, Rgba};
use keyring::{Entry as KeyringEntry, Error as KeyringError};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
  env, fmt,
  fs,
  io::Cursor,
  path::{Path, PathBuf},
  process::Command,
  sync::Mutex,
  thread,
  time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{
  AppHandle, CustomMenuItem, GlobalShortcutManager, Manager, RunEvent, State, SystemTray, SystemTrayEvent,
  SystemTrayMenu, SystemTraySubmenu, TitleBarStyle, Window, WindowEvent, WindowUrl,
};
use url::Url;

const KEYRING_SERVICE: &str = "ca.aahmed.screenshotr";
const KEYRING_R2_ACCESS_KEY_ID: &str = "r2_access_key_id";
const KEYRING_R2_SECRET_ACCESS_KEY: &str = "r2_secret_access_key";
const HISTORY_LIMIT: usize = 1000;
/// Larger files are refused up front rather than failing slowly mid-upload.
const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
// Menu bar icons: template images (black on clear), 36 px for 18 pt at 2x.
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray/idle.png");
const TRAY_ATTENTION: &[u8] = include_bytes!("../icons/tray/attention.png");
const TRAY_DONE: &[u8] = include_bytes!("../icons/tray/done.png");
const TRAY_ERROR: &[u8] = include_bytes!("../icons/tray/error.png");
const TRAY_UPLOAD: [&[u8]; 8] = [
  include_bytes!("../icons/tray/upload-0.png"),
  include_bytes!("../icons/tray/upload-1.png"),
  include_bytes!("../icons/tray/upload-2.png"),
  include_bytes!("../icons/tray/upload-3.png"),
  include_bytes!("../icons/tray/upload-4.png"),
  include_bytes!("../icons/tray/upload-5.png"),
  include_bytes!("../icons/tray/upload-6.png"),
  include_bytes!("../icons/tray/upload-7.png"),
];
const TRAY_RECENT_LIMIT: usize = 6;
// The overlay is a Liquid Glass panel the size of its content, placed like a notification banner.
const HUD_WIDTH: f64 = 344.0;
const HUD_HEIGHT: f64 = 80.0;
const HUD_RADIUS: f64 = 22.0;
const HUD_INSET_RIGHT: f64 = 14.0;
const HUD_INSET_TOP: f64 = 44.0;

#[derive(Clone, Copy)]
enum CaptureMode {
  Full,
  Area,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
struct AppConfig {
  public_base_url: String,
  r2_endpoint: String,
  r2_bucket: String,
  r2_access_key_id: String,
  r2_secret_access_key: String,
  hotkey_full: String,
  hotkey_area: String,
  retain_local_copy: bool,
  open_at_login: bool,
  destinations: Vec<Destination>,
  active_destination: String,
  onboarded: bool,
  /// Uploads whatever is on the clipboard; off unless set.
  hotkey_clipboard: String,
  /// What goes on the clipboard: "url", "markdown" or "html".
  link_format: String,
  /// Look for a new version once a day.
  check_updates: bool,
  /// Play Screenshotr's capture sound (only when macOS sound effects are on too).
  play_sound: bool,
}

impl Default for AppConfig {
  fn default() -> Self {
    let (hotkey_full, hotkey_area) = default_hotkeys();
    Self {
      public_base_url: String::new(),
      r2_endpoint: String::new(),
      r2_bucket: "screenshots".to_string(),
      r2_access_key_id: String::new(),
      r2_secret_access_key: String::new(),
      hotkey_full,
      hotkey_area,
      retain_local_copy: false,
      open_at_login: false,
      destinations: Vec::new(),
      active_destination: String::new(),
      onboarded: false,
      hotkey_clipboard: String::new(),
      link_format: "url".to_string(),
      check_updates: true,
      play_sound: true,
    }
  }
}

struct ConfigState(Mutex<AppConfig>);

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
struct Upload {
  id: String,
  url: String,
  created_at: i64,
  #[serde(skip_serializing_if = "Option::is_none")]
  deletion_url: Option<String>,
  /// What the person would call it: the file's name, or "Screenshot" for captures.
  name: String,
  size: u64,
  mime: String,
  destination_id: String,
  destination_name: String,
  /// Lets Screenshotr delete the object itself (S3-compatible destinations).
  #[serde(skip_serializing_if = "Option::is_none")]
  remote_key: Option<String>,
}

impl Default for Upload {
  fn default() -> Self {
    Self {
      id: random_suffix(12),
      url: String::new(),
      created_at: 0,
      deletion_url: None,
      name: String::new(),
      size: 0,
      mime: String::new(),
      destination_id: String::new(),
      destination_name: String::new(),
      remote_key: None,
    }
  }
}

struct HistoryState(Mutex<Vec<Upload>>);

/// What clicking the overlay does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum HudAction {
  #[default]
  None,
  OpenLink,
  RevealFile,
  OpenStorageSettings,
  OpenScreenRecordingSettings,
  OpenKeyboardSettings,
}

#[derive(Clone, Default, Serialize)]
struct HudState {
  seq: u64,
  phase: String,
  title: String,
  message: String,
  thumb: Option<String>,
  action: HudAction,
  target: Option<String>,
  /// "capture" or "file"; files without a preview show a document glyph.
  kind: String,
}

struct HudStore(Mutex<HudState>);

/// The newest version found by an update check, if it's newer than this one.
#[derive(Default)]
struct UpdateState(Mutex<Option<String>>);

/// Each tray state change bumps the generation; older animations notice and stop.
#[derive(Default)]
struct TrayAnimation {
  generation: std::sync::atomic::AtomicU64,
  busy: std::sync::atomic::AtomicBool,
}

/// An error written for the person using the app, shown in the overlay with a way to fix it.
#[derive(Debug)]
struct Notice {
  title: String,
  message: String,
  action: HudAction,
}

impl fmt::Display for Notice {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.title, self.message)
  }
}

impl std::error::Error for Notice {}

impl Notice {
  fn new(title: &str, message: &str, action: HudAction) -> Self {
    Self { title: title.to_string(), message: message.to_string(), action }
  }

  #[cfg(target_os = "macos")]
  fn screen_recording() -> Self {
    Self::new(
      "Screen Recording is off",
      "Turn it on for Screenshotr, then reopen the app. Click to open System Settings.",
      HudAction::OpenScreenRecordingSettings,
    )
  }

  #[cfg(target_os = "macos")]
  fn shortcut_conflict() -> Self {
    Self::new(
      "macOS took the shortcut",
      "Turn off the Screenshots shortcuts in Keyboard settings. Click to open them.",
      HudAction::OpenKeyboardSettings,
    )
  }

  fn needs_setup() -> Self {
    Self::new(
      "Choose where screenshots go",
      "Pick an image host or your own storage. Click to open Settings.",
      HudAction::OpenStorageSettings,
    )
  }
}

fn main() {
  println!("[startup] Screenshotr booting");
  let app = tauri::Builder::default()
    .system_tray(SystemTray::new().with_menu(SystemTrayMenu::new()))
    .on_system_tray_event(|app, event| handle_tray_event(app, event))
    .on_window_event(|event| {
      if let WindowEvent::CloseRequested { api, .. } = event.event() {
        let _ = event.window().hide();
        api.prevent_close();
      }
    })
    .setup(|app| {
      #[cfg(target_os = "macos")]
      {
        println!("[startup] macos accessory mode enabled");
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);
      }
      let handle = app.handle();
      let (config, exists) = load_config(&handle)?;
      app.manage(ConfigState(Mutex::new(config.clone())));
      app.manage(HistoryState(Mutex::new(load_history(&handle))));
      app.manage(HudStore(Mutex::new(HudState::default())));
      app.manage(TrayAnimation::default());
      app.manage(UpdateState::default());
      println!(
        "[config] public_base_url={} r2_endpoint={} r2_bucket={} hotkey_full={} hotkey_area={}",
        config.public_base_url,
        config.r2_endpoint,
        config.r2_bucket,
        config.hotkey_full,
        config.hotkey_area
      );
      if let Err(error) = ensure_hud_window(&handle) {
        println!("[hud] failed to create overlay: {error}");
      }
      #[cfg(target_os = "macos")]
      exit_on_termination_signal(&handle);
      #[cfg(target_os = "macos")]
      macos_receive_files(&handle);
      #[cfg(target_os = "macos")]
      if let Err(error) = macos_sync_system_shortcuts(&handle, &config) {
        println!("[hotkeys] failed to disable system screenshot shortcuts: {error}");
        show_notice(&handle, &Notice::shortcut_conflict());
      }
      println!("[startup] registering global shortcuts");
      if let Err(error) = register_shortcuts(&handle) {
        // Keep running so the shortcut can be fixed in Settings.
        println!("[hotkeys] registration failed: {error}");
      }
      update_tray(&handle);
      if !config.onboarded && !is_configured(&config) {
        open_welcome_window(&handle)?;
      } else if !exists || !is_configured(&config) {
        open_settings_window(&handle, "storage")?;
      }
      #[cfg(debug_assertions)]
      run_demo(&handle);
      start_update_checks(&handle);
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      get_config,
      set_config,
      get_status,
      test_destination,
      import_sxcu,
      connect_r2,
      save_destination,
      delete_destination,
      hud_state,
      hud_activate,
      hud_dismiss,
      hud_settle,
      hud_drag,
      open_pictures_folder,
      open_system_settings,
      set_hotkeys_paused,
      request_screen_recording,
      relaunch_app,
      finish_onboarding,
      open_settings,
      fit_window_height,
      history_list,
      history_thumbnail,
      history_copy,
      history_open,
      history_remove,
      upload_files,
      upload_chooser,
      check_for_update,
      install_update,
      preview_capture_sound
    ])
    .build(tauri::generate_context!())
    .expect("error while running tauri application");

  app.run(|app_handle, event| match event {
    RunEvent::ExitRequested { .. } => {
      let _ = app_handle.global_shortcut_manager().unregister_all();
    }
    // Reached when macOS asks the app to quit (Cmd+Q, logout, Activity Monitor).
    RunEvent::Exit => release_hotkeys(app_handle),
    _ => {}
  });
}

/// `AppHandle::exit` ends the process without emitting `RunEvent::Exit`, so clean up first.
fn quit(app: &AppHandle) {
  release_hotkeys(app);
  app.exit(0);
}

fn release_hotkeys(app: &AppHandle) {
  let _ = app.global_shortcut_manager().unregister_all();
  #[cfg(target_os = "macos")]
  if let Err(error) = macos_restore_system_shortcuts(app) {
    println!("[hotkeys] failed to restore system screenshot shortcuts: {error}");
  }
}

// ---------------------------------------------------------------------------------------------
// Tray

fn build_tray_menu(app: &AppHandle, config: &AppConfig, history: &[Upload]) -> SystemTrayMenu {
  let manager = app.global_shortcut_manager();
  // Only registered hotkeys are known to parse; an invalid accelerator would panic in the menu.
  let with_hotkey = |item: CustomMenuItem, hotkey: &str| {
    if !hotkey.is_empty() && manager.is_registered(hotkey).unwrap_or(false) {
      item.accelerator(hotkey)
    } else {
      item
    }
  };
  let (full, area) = current_hotkeys(config);
  let mut menu = SystemTrayMenu::new();

  if !is_configured(config) {
    menu = menu
      .add_item(CustomMenuItem::new("setup", "Finish setup…"))
      .add_native_item(tauri::SystemTrayMenuItem::Separator);
  }

  menu = menu
    .add_item(with_hotkey(CustomMenuItem::new("capture_area", "Capture area"), area))
    .add_item(with_hotkey(CustomMenuItem::new("capture_full", "Capture screen"), full))
    .add_native_item(tauri::SystemTrayMenuItem::Separator)
    .add_item(CustomMenuItem::new("upload_files", "Upload Files…"))
    .add_item(with_hotkey(CustomMenuItem::new("upload_clipboard", "Upload Clipboard"), &config.hotkey_clipboard))
    .add_native_item(tauri::SystemTrayMenuItem::Separator);

  let copy_last = CustomMenuItem::new("copy_last", "Copy last link");
  menu = menu.add_item(if history.is_empty() { copy_last.disabled() } else { copy_last });
  if !history.is_empty() {
    let mut recent = SystemTrayMenu::new();
    for (index, upload) in history.iter().take(TRAY_RECENT_LIMIT).enumerate() {
      recent = recent.add_item(CustomMenuItem::new(format!("recent:{index}"), recent_label(upload)));
    }
    menu = menu.add_submenu(SystemTraySubmenu::new("Recent uploads", recent));
  }
  menu = menu.add_item(CustomMenuItem::new("history", "Upload History…").accelerator("CmdOrCtrl+Y"));

  if let Some(version) = app.state::<UpdateState>().0.lock().unwrap().clone() {
    menu = menu
      .add_native_item(tauri::SystemTrayMenuItem::Separator)
      .add_item(CustomMenuItem::new("install_update", format!("Install Screenshotr {version} and Relaunch")));
  }
  menu
    .add_native_item(tauri::SystemTrayMenuItem::Separator)
    .add_item(CustomMenuItem::new("settings", "Settings…").accelerator("CmdOrCtrl+,"))
    .add_item(CustomMenuItem::new("about", "About Screenshotr"))
    .add_native_item(tauri::SystemTrayMenuItem::Separator)
    .add_item(CustomMenuItem::new("quit", "Quit Screenshotr").accelerator("CmdOrCtrl+Q"))
}

/// "Today at 17:32 · screenshot-…png" style labels, without the random suffix noise.
fn recent_label(upload: &Upload) -> String {
  use chrono::{Local, TimeZone};

  let when = Local
    .timestamp_opt(upload.created_at, 0)
    .single()
    .map(|time| {
      let today = Local::now().date_naive();
      if time.date_naive() == today {
        format!("Today at {}", time.format("%H:%M"))
      } else if Some(time.date_naive()) == today.pred_opt() {
        format!("Yesterday at {}", time.format("%H:%M"))
      } else {
        time.format("%b %-d at %H:%M").to_string()
      }
    })
    .unwrap_or_else(|| "Earlier".to_string());
  let host = display_host(&upload.url);
  format!("{when}  —  {host}")
}

fn update_tray(app: &AppHandle) {
  let config = app.state::<ConfigState>().0.lock().unwrap().clone();
  let history = app.state::<HistoryState>().0.lock().unwrap().clone();
  if let Err(error) = app.tray_handle().set_menu(build_tray_menu(app, &config, &history)) {
    println!("[tray] failed to update menu: {error}");
  }
  if !app.state::<TrayAnimation>().busy.load(std::sync::atomic::Ordering::SeqCst) {
    tray_rest(app);
  }
}

fn set_tray_icon(app: &AppHandle, png: &[u8]) {
  if let Err(error) = app.tray_handle().set_icon(tauri::Icon::Raw(png.to_vec())) {
    println!("[tray] failed to set icon: {error}");
  }
}

/// Starts a new tray state and returns its generation.
fn tray_begin(app: &AppHandle, busy: bool) -> u64 {
  use std::sync::atomic::Ordering;
  let animation = app.state::<TrayAnimation>();
  animation.busy.store(busy, Ordering::SeqCst);
  animation.generation.fetch_add(1, Ordering::SeqCst) + 1
}

fn tray_is_current(app: &AppHandle, generation: u64) -> bool {
  app.state::<TrayAnimation>().generation.load(std::sync::atomic::Ordering::SeqCst) == generation
}

/// The icon at rest: plain, or with a dot when there's nowhere to upload yet.
fn tray_rest(app: &AppHandle) {
  tray_begin(app, false);
  let configured = is_configured(&app.state::<ConfigState>().0.lock().unwrap());
  set_tray_icon(app, if configured { TRAY_IDLE } else { TRAY_ATTENTION });
  let tooltip = if configured { "Screenshotr" } else { "Screenshotr: choose where screenshots go" };
  let _ = app.tray_handle().set_tooltip(tooltip);
}

/// Loops the upload frames until another tray state replaces it.
fn tray_uploading(app: &AppHandle) {
  let generation = tray_begin(app, true);
  let _ = app.tray_handle().set_tooltip("Screenshotr: uploading…");
  let app = app.clone();
  thread::spawn(move || {
    for frame in TRAY_UPLOAD.iter().cycle() {
      if !tray_is_current(&app, generation) {
        return;
      }
      set_tray_icon(&app, frame);
      thread::sleep(Duration::from_millis(110));
    }
  });
}

/// Shows a result icon briefly, then returns to rest unless something newer took over.
fn tray_flash(app: &AppHandle, png: &'static [u8], duration: Duration) {
  let generation = tray_begin(app, true);
  set_tray_icon(app, png);
  let app = app.clone();
  thread::spawn(move || {
    thread::sleep(duration);
    if tray_is_current(&app, generation) {
      tray_rest(&app);
    }
  });
}

fn handle_tray_event(app: &AppHandle, event: SystemTrayEvent) {
  let SystemTrayEvent::MenuItemClick { id, .. } = event else {
    return;
  };
  let open = |tab: &str| {
    if let Err(error) = open_settings_window(app, tab) {
      println!("[error] failed to open settings: {error}");
    }
  };
  match id.as_str() {
    "setup" => open("storage"),
    "settings" => open("general"),
    "about" => open("about"),
    "capture_area" => spawn_capture(app.clone(), CaptureMode::Area),
    "capture_full" => spawn_capture(app.clone(), CaptureMode::Full),
    "copy_last" => copy_recent(app, 0),
    "upload_files" => choose_files(app),
    "upload_clipboard" => upload_clipboard(app),
    "install_update" => {
      let app = app.clone();
      tauri::async_runtime::spawn(async move {
        if let Err(error) = install_update(app.clone()).await {
          show_notice(&app, &Notice::new("Update failed", &error, HudAction::None));
        }
      });
    }
    "history" => {
      if let Err(error) = open_history_window(app) {
        println!("[error] failed to open history: {error}");
      }
    }
    "quit" => quit(app),
    other => {
      if let Some(index) = other.strip_prefix("recent:").and_then(|index| index.parse().ok()) {
        copy_recent(app, index);
      }
    }
  }
}

fn copy_recent(app: &AppHandle, index: usize) {
  let upload = app.state::<HistoryState>().0.lock().unwrap().get(index).cloned();
  let Some(upload) = upload else {
    return;
  };
  let format = app.state::<ConfigState>().0.lock().unwrap().link_format.clone();
  let (text, title) = formatted_link(&upload.url, &format);
  if copy_to_clipboard(&text).is_ok() {
    show_hud(
      app,
      HudState {
        phase: "done".to_string(),
        title: title.to_string(),
        message: display_link(&upload.url),
        action: HudAction::OpenLink,
        target: Some(upload.url),
        ..HudState::default()
      },
    );
  }
}

// ---------------------------------------------------------------------------------------------
// Capture

fn register_shortcuts(app: &AppHandle) -> Result<()> {
  println!("[hotkeys] registering");
  let mut manager = app.global_shortcut_manager();
  let config = app.state::<ConfigState>();
  let config = config.0.lock().unwrap().clone();
  let (full, area) = current_hotkeys(&config);

  if !full.is_empty() {
    let app_handle = app.clone();
    let full_label = full.to_string();
    manager
      .register(full, move || {
        println!("[hotkeys] {full_label} pressed");
        spawn_capture(app_handle.clone(), CaptureMode::Full);
      })
      .with_context(|| format!("couldn't register {full}"))?;
  }

  if !area.is_empty() {
    let app_handle = app.clone();
    let area_label = area.to_string();
    manager
      .register(area, move || {
        println!("[hotkeys] {area_label} pressed");
        spawn_capture(app_handle.clone(), CaptureMode::Area);
      })
      .with_context(|| format!("couldn't register {area}"))?;
  }

  let clipboard = config.hotkey_clipboard.trim();
  if !clipboard.is_empty() {
    let app_handle = app.clone();
    manager
      .register(clipboard, move || upload_clipboard(&app_handle))
      .with_context(|| format!("couldn't register {clipboard}"))?;
  }

  println!("[hotkeys] registration complete");
  Ok(())
}

fn spawn_capture(app: AppHandle, mode: CaptureMode) {
  thread::spawn(move || {
    println!("[capture] capture thread started");
    if let Err(error) = capture_upload_and_copy(&app, mode) {
      println!("[error] capture failed: {error:#}");
      match error.downcast_ref::<Notice>() {
        Some(notice) => show_notice(&app, notice),
        None => show_notice(&app, &Notice::new("Screenshot failed", &error.to_string(), HudAction::None)),
      }
    }
  });
}

fn capture_upload_and_copy(app: &AppHandle, mode: CaptureMode) -> Result<()> {
  let config = app.state::<ConfigState>().0.lock().unwrap().clone();
  // Check before capturing so nobody frames a shot that can't go anywhere.
  let Some(destination) = active_destination(&config) else {
    return Err(Notice::needs_setup().into());
  };

  println!("[capture] starting");
  let Some(capture) = capture_screenshot(mode)? else {
    println!("[capture] cancelled by user");
    return Ok(());
  };
  println!("[capture] bytes captured: {}", capture.bytes.len());
  if config.play_sound {
    play_capture_sound(app);
  }

  let thumb = thumbnail_data_url(&capture.bytes).ok();
  let kept = if config.retain_local_copy { save_to_pictures(&capture.bytes).ok() } else { None };
  let item = Outgoing { name: "Screenshot".to_string(), file_name: object_key(&None), mime: "image/png".to_string(), bytes: capture.bytes };
  if let Err(error) = deliver(app, &config, &destination, std::slice::from_ref(&item), thumb.clone(), "capture") {
    println!("[upload] failed: {error:#}");
    tray_flash(app, TRAY_ERROR, Duration::from_secs(4));
    // Never lose a capture: fall back to a copy in Pictures.
    let rescued = kept.or_else(|| save_to_pictures(&item.bytes).ok());
    let message = match &rescued {
      Some(_) => format!("{error}. The screenshot is saved in Pictures. Click to show it."),
      None => format!("{error}."),
    };
    show_hud(
      app,
      HudState {
        phase: "error".to_string(),
        title: "Upload failed".to_string(),
        message,
        thumb,
        action: if rescued.is_some() { HudAction::RevealFile } else { HudAction::OpenStorageSettings },
        target: rescued.map(|path| path.display().to_string()),
        ..HudState::default()
      },
    );
  }
  remove_temp_file(&capture.path);
  Ok(())
}

/// One thing to upload: a capture, a file, or an image from the clipboard.
struct Outgoing {
  /// The name shown in history.
  name: String,
  /// The name it's stored under at the destination.
  file_name: String,
  mime: String,
  bytes: Vec<u8>,
}

/// Uploads items to the destination, records them, puts their links on the clipboard in the
/// chosen format and shows the result. Every way into the app (captures, files, the clipboard,
/// Finder) ends here, so they all behave alike.
fn deliver(
  app: &AppHandle,
  config: &AppConfig,
  destination: &Destination,
  items: &[Outgoing],
  thumb: Option<String>,
  kind: &str,
) -> Result<Vec<String>> {
  let what = match items {
    [one] if kind == "file" => one.name.clone(),
    [_] => String::new(),
    many => format!("{} files", many.len()),
  };
  tray_uploading(app);
  show_hud(
    app,
    HudState {
      phase: "uploading".to_string(),
      title: "Uploading".to_string(),
      message: if what.is_empty() { format!("to {}", destination.host_label()) } else { format!("{what} to {}", destination.host_label()) },
      thumb: thumb.clone(),
      kind: kind.to_string(),
      ..HudState::default()
    },
  );

  let mut links = Vec::new();
  for item in items {
    println!("[upload] {} ({} bytes) to {}", item.file_name, item.bytes.len(), destination.name);
    let uploaded = tauri::async_runtime::block_on(destinations::upload(destination, &item.bytes, &item.file_name, &item.mime))
      .map_err(|error| if items.len() > 1 { anyhow!("{}: {error}", item.name) } else { error })?;
    let preview = if item.mime.starts_with("image/") { history_thumbnail_png(&item.bytes).ok() } else { None };
    record_upload(
      app,
      Upload {
        url: uploaded.url.clone(),
        created_at: chrono::Utc::now().timestamp(),
        deletion_url: uploaded.deletion_url,
        name: item.name.clone(),
        size: item.bytes.len() as u64,
        mime: item.mime.clone(),
        destination_id: destination.id.clone(),
        destination_name: destination.name.clone(),
        remote_key: uploaded.remote_key,
        ..Upload::default()
      },
      preview,
    );
    let _ = app.emit_all("upload-finished", &uploaded.url);
    links.push(uploaded.url);
  }

  let formatted: Vec<(String, &str)> = links.iter().map(|url| formatted_link(url, &config.link_format)).collect();
  copy_to_clipboard(&formatted.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>().join("\n"))?;
  tray_flash(app, TRAY_DONE, Duration::from_millis(1800));
  let (title, message, target) = match links.as_slice() {
    [only] => (formatted[0].1.to_string(), display_link(only), Some(only.clone())),
    many => (format!("{} links copied", many.len()), items.iter().map(|item| item.name.as_str()).collect::<Vec<_>>().join(", "), None),
  };
  show_hud(
    app,
    HudState {
      phase: "done".to_string(),
      title,
      message,
      thumb,
      action: if target.is_some() { HudAction::OpenLink } else { HudAction::None },
      target,
      kind: kind.to_string(),
      ..HudState::default()
    },
  );
  Ok(links)
}

/// Uploads files chosen in a dialog, dropped on a window, opened from Finder or sent from the
/// Services menu.
fn upload_paths(app: &AppHandle, paths: Vec<PathBuf>) {
  let app = app.clone();
  thread::spawn(move || {
    let config = app.state::<ConfigState>().0.lock().unwrap().clone();
    let Some(destination) = active_destination(&config) else {
      return show_notice(&app, &Notice::needs_setup());
    };
    let mut items = Vec::new();
    for path in paths.iter().filter(|path| path.is_file()) {
      let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());
      let size = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
      if size > MAX_UPLOAD_BYTES {
        return show_notice(&app, &Notice::new("File too large", &format!("{name} is over 512 MB."), HudAction::None));
      }
      match fs::read(path) {
        Ok(bytes) => items.push(Outgoing { file_name: stored_file_name(&name), mime: mime_for(path), name, bytes }),
        Err(error) => return show_notice(&app, &Notice::new("Couldn't read the file", &format!("{name}: {error}"), HudAction::None)),
      }
    }
    if items.is_empty() {
      return show_notice(&app, &Notice::new("Nothing to upload", "Folders can't be uploaded; choose files instead.", HudAction::None));
    }
    if !destinations::accepts_any_file(&destination) && items.iter().any(|item| !item.mime.starts_with("image/")) {
      return show_notice(&app, &Notice::new(&format!("{} takes images only", destination.name), "Pick another destination in Settings for other files.", HudAction::OpenStorageSettings));
    }
    let thumb = items.iter().find(|item| item.mime.starts_with("image/")).and_then(|item| thumbnail_data_url(&item.bytes).ok());
    if let Err(error) = deliver(&app, &config, &destination, &items, thumb, "file") {
      println!("[upload] failed: {error:#}");
      tray_flash(&app, TRAY_ERROR, Duration::from_secs(4));
      show_notice(&app, &Notice::new("Upload failed", &format!("{error}."), HudAction::None));
    }
  });
}

/// Uploads whatever is on the clipboard: copied files first, otherwise an image.
fn upload_clipboard(app: &AppHandle) {
  #[cfg(target_os = "macos")]
  {
    let paths = macos_pasteboard_files();
    if !paths.is_empty() {
      return upload_paths(app, paths);
    }
  }
  let app = app.clone();
  thread::spawn(move || {
    let image = Clipboard::new().ok().and_then(|mut clipboard| clipboard.get_image().ok());
    let Some(image) = image else {
      return show_notice(&app, &Notice::new("Nothing to upload", "Copy an image or some files first.", HudAction::None));
    };
    let Some(buffer) = ImageBuffer::<Rgba<u8>, _>::from_raw(image.width as u32, image.height as u32, image.bytes.into_owned()) else {
      return show_notice(&app, &Notice::new("Couldn't read the clipboard", "The copied image is in a format Screenshotr can't read.", HudAction::None));
    };
    let mut png = Vec::new();
    if buffer.write_to(&mut Cursor::new(&mut png), image::ImageOutputFormat::Png).is_err() {
      return;
    }
    let config = app.state::<ConfigState>().0.lock().unwrap().clone();
    let Some(destination) = active_destination(&config) else {
      return show_notice(&app, &Notice::needs_setup());
    };
    let thumb = thumbnail_data_url(&png).ok();
    let item = Outgoing { name: "Clipboard image".to_string(), file_name: object_key(&None), mime: "image/png".to_string(), bytes: png };
    if let Err(error) = deliver(&app, &config, &destination, std::slice::from_ref(&item), thumb, "capture") {
      tray_flash(&app, TRAY_ERROR, Duration::from_secs(4));
      show_notice(&app, &Notice::new("Upload failed", &format!("{error}."), HudAction::None));
    }
  });
}

fn choose_files(app: &AppHandle) {
  let app = app.clone();
  tauri::api::dialog::FileDialogBuilder::new().set_title("Upload Files").pick_files(move |paths| {
    if let Some(paths) = paths {
      upload_paths(&app, paths);
    }
  });
}

/// Files copied in Finder sit on the pasteboard as file URLs, which arboard can't read.
#[cfg(target_os = "macos")]
fn macos_pasteboard_files() -> Vec<PathBuf> {
  use cocoa::base::id;
  unsafe {
    let pasteboard: id = msg_send![class!(NSPasteboard), generalPasteboard];
    macos_file_urls_on(pasteboard)
  }
}

#[cfg(target_os = "macos")]
unsafe fn macos_file_urls_on(pasteboard: cocoa::base::id) -> Vec<PathBuf> {
  use cocoa::base::{id, YES};
  let classes: id = msg_send![class!(NSArray), arrayWithObject: class!(NSURL)];
  let key: id = msg_send![class!(NSString), stringWithUTF8String: "NSPasteboardURLReadingFileURLsOnlyKey\0".as_ptr()];
  let yes: id = msg_send![class!(NSNumber), numberWithBool: YES];
  let options: id = msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: key];
  let urls: id = msg_send![pasteboard, readObjectsForClasses: classes options: options];
  macos_paths_from_urls(urls)
}

#[cfg(target_os = "macos")]
unsafe fn macos_paths_from_urls(urls: cocoa::base::id) -> Vec<PathBuf> {
  use cocoa::base::{id, nil};
  if urls == nil {
    return Vec::new();
  }
  let count: usize = msg_send![urls, count];
  (0..count)
    .filter_map(|index| {
      let url: id = msg_send![urls, objectAtIndex: index];
      let path: id = msg_send![url, path];
      if path == nil {
        return None;
      }
      let utf8: *const std::os::raw::c_char = msg_send![path, UTF8String];
      (!utf8.is_null()).then(|| PathBuf::from(std::ffi::CStr::from_ptr(utf8).to_string_lossy().to_string()))
    })
    .collect()
}

/// Finder's Open With, dropping files on the app icon, and the Services menu all arrive through
/// AppKit, which Tauri doesn't surface. Add the two handlers to the app delegate and to a small
/// services provider; both hand the files to upload_paths.
#[cfg(target_os = "macos")]
fn macos_receive_files(app: &AppHandle) {
  use objc::declare::ClassDecl;
  use objc::runtime::{Class, Object, Sel};
  use std::sync::OnceLock;

  static APP: OnceLock<AppHandle> = OnceLock::new();
  let _ = APP.set(app.clone());

  extern "C" fn open_urls(_this: &Object, _sel: Sel, _application: cocoa::base::id, urls: cocoa::base::id) {
    let paths = unsafe { macos_paths_from_urls(urls) };
    if let (Some(app), false) = (APP.get(), paths.is_empty()) {
      upload_paths(app, paths);
    }
  }

  extern "C" fn upload_service(_this: &Object, _sel: Sel, pasteboard: cocoa::base::id, _data: cocoa::base::id, _error: *mut std::ffi::c_void) {
    let paths = unsafe { macos_file_urls_on(pasteboard) };
    if let (Some(app), false) = (APP.get(), paths.is_empty()) {
      upload_paths(app, paths);
    }
  }

  let app = app.clone();
  let _ = app.clone().run_on_main_thread(move || unsafe {
    use cocoa::base::id;
    let shared: id = msg_send![class!(NSApplication), sharedApplication];
    let delegate: id = msg_send![shared, delegate];
    if !delegate.is_null() {
      let delegate_class = objc::runtime::object_getClass(delegate as *const Object) as *mut Class;
      objc::runtime::class_addMethod(
        delegate_class,
        sel!(application:openURLs:),
        std::mem::transmute::<extern "C" fn(&Object, Sel, id, id), objc::runtime::Imp>(open_urls),
        "v@:@@\0".as_ptr() as *const std::os::raw::c_char,
      );
    }
    let provider_class = match Class::get("ScreenshotrServices") {
      Some(class) => class,
      None => {
        let mut decl = ClassDecl::new("ScreenshotrServices", class!(NSObject)).expect("services class");
        decl.add_method(sel!(uploadFiles:userData:error:), upload_service as extern "C" fn(&Object, Sel, id, id, *mut std::ffi::c_void));
        decl.register()
      }
    };
    let provider: id = msg_send![provider_class, new];
    let _: () = msg_send![shared, setServicesProvider: provider];
    NSUpdateDynamicServices();
    let _ = &app;
  });
}

#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
extern "C" {
  fn NSUpdateDynamicServices();
}

fn mime_for(path: &Path) -> String {
  mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string()
}

/// "Quarterly Report.pdf" becomes "quarterly-report-k2Jd8fQa.pdf": readable, URL-safe and unique.
fn stored_file_name(name: &str) -> String {
  let (stem, ext) = match name.rsplit_once('.') {
    Some((stem, ext)) if !stem.is_empty() && ext.len() <= 8 => (stem, Some(ext)),
    _ => (name, None),
  };
  let mut slug = String::new();
  for c in stem.chars() {
    if c.is_ascii_alphanumeric() {
      slug.push(c.to_ascii_lowercase());
    } else if !slug.ends_with('-') && !slug.is_empty() {
      slug.push('-');
    }
  }
  let slug: String = slug.trim_matches('-').chars().take(48).collect();
  let slug = if slug.is_empty() { "file".to_string() } else { slug };
  match ext {
    Some(ext) => format!("{slug}-{}.{}", random_suffix(8), ext.to_ascii_lowercase()),
    None => format!("{slug}-{}", random_suffix(8)),
  }
}

fn remove_temp_file(path: &Option<PathBuf>) {
  if let Some(path) = path {
    let _ = fs::remove_file(path);
  }
}

fn thumbnail_data_url(bytes: &[u8]) -> Result<String> {
  let image = image::load_from_memory(bytes).context("failed to decode screenshot")?;
  // 2x the overlay's thumbnail box so it stays sharp on Retina displays.
  let thumb = image.thumbnail(176, 112);
  let mut png = Vec::new();
  thumb
    .write_to(&mut Cursor::new(&mut png), image::ImageOutputFormat::Png)
    .context("failed to encode thumbnail")?;
  Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

fn pictures_dir() -> Option<PathBuf> {
  tauri::api::path::picture_dir().map(|dir| dir.join("Screenshotr"))
}

/// Saves with the same naming macOS uses for its own screenshots.
fn save_to_pictures(bytes: &[u8]) -> Result<PathBuf> {
  let dir = pictures_dir().ok_or_else(|| anyhow!("no Pictures folder"))?;
  fs::create_dir_all(&dir).context("failed to create Pictures/Screenshotr")?;
  let stamp = chrono::Local::now().format("Screenshot %Y-%m-%d at %H.%M.%S");
  let mut path = dir.join(format!("{stamp}.png"));
  let mut counter = 2;
  while path.exists() {
    path = dir.join(format!("{stamp} ({counter}).png"));
    counter += 1;
  }
  fs::write(&path, bytes).context("failed to save screenshot")?;
  Ok(path)
}

fn load_history(app: &AppHandle) -> Vec<Upload> {
  let mut history: Vec<Upload> = history_path(app)
    .ok()
    .and_then(|path| fs::read_to_string(path).ok())
    .and_then(|contents| serde_json::from_str(&contents).ok())
    .unwrap_or_default();
  for upload in &mut history {
    if upload.name.is_empty() {
      upload.name = "Screenshot".to_string();
    }
  }
  history
}

fn history_path(app: &AppHandle) -> Result<PathBuf> {
  Ok(config_path(app)?.with_file_name("history.json"))
}

fn record_upload(app: &AppHandle, upload: Upload, thumbnail: Option<Vec<u8>>) {
  if let (Some(png), Ok(dir)) = (thumbnail, thumbnails_dir(app)) {
    let _ = fs::write(dir.join(format!("{}.png", upload.id)), png);
  }
  let dropped = {
    let state = app.state::<HistoryState>();
    let mut history = state.0.lock().unwrap();
    history.insert(0, upload);
    let keep = HISTORY_LIMIT.min(history.len());
    history.split_off(keep)
  };
  for old in dropped {
    if let Ok(dir) = thumbnails_dir(app) {
      let _ = fs::remove_file(dir.join(format!("{}.png", old.id)));
    }
  }
  save_history(app);
  update_tray(app);
  let _ = app.emit_all("history-changed", ());
}

fn save_history(app: &AppHandle) {
  let history = app.state::<HistoryState>().0.lock().unwrap().clone();
  if let Ok(path) = history_path(app) {
    if let Ok(contents) = serde_json::to_string_pretty(&history) {
      let _ = fs::write(path, contents);
    }
  }
}

fn thumbnails_dir(app: &AppHandle) -> Result<PathBuf> {
  let dir = config_path(app)?.with_file_name("thumbnails");
  fs::create_dir_all(&dir).context("failed to create the thumbnails folder")?;
  Ok(dir)
}

/// A small PNG kept for the history window; large enough for its grid at 2x.
fn history_thumbnail_png(bytes: &[u8]) -> Result<Vec<u8>> {
  let image = image::load_from_memory(bytes).context("failed to decode image")?;
  let mut png = Vec::new();
  image
    .thumbnail(480, 320)
    .write_to(&mut Cursor::new(&mut png), image::ImageOutputFormat::Png)
    .context("failed to encode thumbnail")?;
  Ok(png)
}

// ---------------------------------------------------------------------------------------------
// Overlay

fn ensure_hud_window(app: &AppHandle) -> Result<Window> {
  if let Some(window) = app.get_window("hud") {
    return Ok(window);
  }
  let window = tauri::WindowBuilder::new(app, "hud", window_url("hud.html"))
    .title("Screenshotr")
    .inner_size(HUD_WIDTH, HUD_HEIGHT)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .visible(false)
    .accept_first_mouse(true)
    .build()?;
  #[cfg(target_os = "macos")]
  {
    let target = window.clone();
    let _ = window.run_on_main_thread(move || {
      if let Ok(ns_window) = target.ns_window() {
        // Visible on every Space and over full-screen apps.
        let behavior: u64 = (1 << 0) | (1 << 4) | (1 << 8);
        unsafe {
          let ns_window = ns_window as cocoa::base::id;
          let _: () = msg_send![ns_window, setCollectionBehavior: behavior];
          macos_install_glass(ns_window, HUD_RADIUS);
        }
      }
    });
  }
  Ok(window)
}

/// Puts the system material behind the transparent web content: Liquid Glass where the OS has it,
/// the HUD visual effect material before that. The window shadow then follows its rounded shape.
#[cfg(target_os = "macos")]
unsafe fn macos_install_glass(ns_window: cocoa::base::id, radius: f64) {
  use cocoa::base::{id, nil, YES};
  use cocoa::foundation::NSRect;

  let content: id = msg_send![ns_window, contentView];
  let bounds: NSRect = msg_send![content, bounds];
  let material: id = match objc::runtime::Class::get("NSGlassEffectView") {
    Some(glass) => {
      let view: id = msg_send![glass, alloc];
      let view: id = msg_send![view, initWithFrame: bounds];
      let _: () = msg_send![view, setCornerRadius: radius];
      view
    }
    None => {
      let view: id = msg_send![class!(NSVisualEffectView), alloc];
      let view: id = msg_send![view, initWithFrame: bounds];
      // NSVisualEffectMaterialHUDWindow, always active, blended with what's behind the window.
      let _: () = msg_send![view, setMaterial: 13i64];
      let _: () = msg_send![view, setState: 1i64];
      let _: () = msg_send![view, setBlendingMode: 0i64];
      let _: () = msg_send![view, setWantsLayer: YES];
      let layer: id = msg_send![view, layer];
      let _: () = msg_send![layer, setCornerRadius: radius];
      let _: () = msg_send![layer, setMasksToBounds: YES];
      view
    }
  };
  // Width and height sizable, below the web view.
  let _: () = msg_send![material, setAutoresizingMask: 18u64];
  let _: () = msg_send![content, addSubview: material positioned: -1i64 relativeTo: nil];
  let _: () = msg_send![ns_window, setHasShadow: YES];
}

/// Slides the overlay in from the screen edge, or out to it, the way notification banners move.
/// The glass is native, so this is an AppKit frame animation with a custom timing curve.
#[cfg(target_os = "macos")]
fn macos_slide_hud(window: &Window, show: bool) {
  let target = window.clone();
  let _ = window.run_on_main_thread(move || {
    let Ok(ns_window) = target.ns_window() else {
      return;
    };
    unsafe {
      use cocoa::base::{id, NO, YES};
      use cocoa::foundation::{NSPoint, NSRect, NSSize};
      let ns_window = ns_window as id;
      // The primary display (the one with the menu bar) is first in NSScreen.screens.
      let screens: id = msg_send![class!(NSScreen), screens];
      let primary: id = msg_send![screens, objectAtIndex: 0usize];
      let screen: NSRect = msg_send![primary, frame];
      let resting = NSRect::new(
        NSPoint::new(
          screen.origin.x + screen.size.width - HUD_WIDTH - HUD_INSET_RIGHT,
          screen.origin.y + screen.size.height - HUD_INSET_TOP - HUD_HEIGHT,
        ),
        NSSize::new(HUD_WIDTH, HUD_HEIGHT),
      );
      let offset = |dx: f64| NSRect::new(NSPoint::new(resting.origin.x + dx, resting.origin.y), resting.size);

      let visible: bool = msg_send![ns_window, isVisible];
      let alpha: f64 = msg_send![ns_window, alphaValue];
      if show && (!visible || alpha < 0.05) {
        let _: () = msg_send![ns_window, setFrame: offset(48.0) display: NO];
        let _: () = msg_send![ns_window, setAlphaValue: 0.0f64];
        let _: () = msg_send![ns_window, orderFrontRegardless];
      }

      // Arrive on a long decelerating curve; leave on a short accelerating one.
      // Leaving always continues rightwards from wherever the overlay is, including mid-swipe.
      let current: NSRect = msg_send![ns_window, frame];
      let away = offset((current.origin.x - resting.origin.x).max(0.0) + 36.0);
      let (duration, points, frame, to_alpha) =
        if show { (0.55, (0.16f32, 1.0f32, 0.3f32, 1.0f32), resting, 1.0) } else { (0.24, (0.4, 0.0, 1.0, 1.0), away, 0.0) };
      let curve: id = objc::Message::send_message(
        class!(CAMediaTimingFunction),
        objc::runtime::Sel::register("functionWithControlPoints::::"),
        points,
      )
      .unwrap_or(std::ptr::null_mut());
      let _: () = msg_send![class!(NSAnimationContext), beginGrouping];
      let context: id = msg_send![class!(NSAnimationContext), currentContext];
      let _: () = msg_send![context, setDuration: duration];
      if !curve.is_null() {
        let _: () = msg_send![context, setTimingFunction: curve];
      }
      let animator: id = msg_send![ns_window, animator];
      let _: () = msg_send![animator, setFrame: frame display: YES];
      let _: () = msg_send![animator, setAlphaValue: to_alpha];
      let _: () = msg_send![class!(NSAnimationContext), endGrouping];
    }
  });
}

/// Fades the overlay out, then hides it unless a newer overlay arrived meanwhile.
fn hide_hud(app: &AppHandle, seq: u64) {
  let Some(window) = app.get_window("hud") else {
    return;
  };
  #[cfg(target_os = "macos")]
  {
    macos_slide_hud(&window, false);
    let app = app.clone();
    thread::spawn(move || {
      thread::sleep(Duration::from_millis(280));
      if app.state::<HudStore>().0.lock().unwrap().seq == seq {
        let _ = window.hide();
      }
    });
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (seq, app);
    let _ = window.hide();
  }
}

fn show_notice(app: &AppHandle, notice: &Notice) {
  tray_flash(app, TRAY_ERROR, Duration::from_secs(4));
  show_hud(
    app,
    HudState {
      phase: "error".to_string(),
      title: notice.title.clone(),
      message: notice.message.clone(),
      action: notice.action,
      ..HudState::default()
    },
  );
}

fn show_hud(app: &AppHandle, mut state: HudState) {
  {
    let store = app.state::<HudStore>();
    let mut current = store.0.lock().unwrap();
    state.seq = current.seq + 1;
    *current = state.clone();
  }
  let window = match ensure_hud_window(app) {
    Ok(window) => window,
    Err(error) => {
      println!("[hud] unavailable: {error}");
      return;
    }
  };
  let _ = window.emit("hud", state);
  present_hud(&window);
}

/// Shows the overlay in the top-right corner without taking focus from the app in front.
fn present_hud(window: &Window) {
  // An overlay that's already showing just changes its content; macOS slides in a new one.
  #[cfg(target_os = "macos")]
  macos_slide_hud(window, true);
  #[cfg(not(target_os = "macos"))]
  {
    if let Ok(Some(monitor)) = window.primary_monitor() {
      let scale = monitor.scale_factor();
      let x = monitor.position().x as f64 + monitor.size().width as f64 - (HUD_WIDTH + HUD_INSET_RIGHT) * scale;
      let y = monitor.position().y as f64 + HUD_INSET_TOP * scale;
      let _ = window.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
    }
    let _ = window.show();
  }
}

/// Puts a dragged overlay back in its place, on the same curve it arrives with.
#[tauri::command]
fn hud_settle(app: AppHandle) {
  if let Some(window) = app.get_window("hud") {
    present_hud(&window);
  }
}

/// Moves the overlay horizontally while it's being swiped, relative to its resting place.
#[tauri::command]
fn hud_drag(app: AppHandle, dx: f64) {
  #[cfg(target_os = "macos")]
  if let Some(window) = app.get_window("hud") {
    let target = window.clone();
    let _ = window.run_on_main_thread(move || {
      let Ok(ns_window) = target.ns_window() else {
        return;
      };
      unsafe {
        use cocoa::base::{id, NO};
        use cocoa::foundation::{NSPoint, NSRect, NSSize};
        let ns_window = ns_window as id;
        let screens: id = msg_send![class!(NSScreen), screens];
        let primary: id = msg_send![screens, objectAtIndex: 0usize];
        let screen: NSRect = msg_send![primary, frame];
        let frame = NSRect::new(
          NSPoint::new(
            screen.origin.x + screen.size.width - HUD_WIDTH - HUD_INSET_RIGHT + dx,
            screen.origin.y + screen.size.height - HUD_INSET_TOP - HUD_HEIGHT,
          ),
          NSSize::new(HUD_WIDTH, HUD_HEIGHT),
        );
        let _: () = msg_send![ns_window, setFrame: frame display: NO];
      }
    });
  }
  #[cfg(not(target_os = "macos"))]
  let _ = (app, dx);
}

#[tauri::command]
fn hud_state(store: State<HudStore>) -> HudState {
  store.0.lock().unwrap().clone()
}

#[tauri::command]
fn hud_dismiss(app: AppHandle, seq: u64) {
  // A newer capture may already be showing; only hide the one that timed out.
  if app.state::<HudStore>().0.lock().unwrap().seq != seq {
    return;
  }
  hide_hud(&app, seq);
}

#[tauri::command]
fn hud_activate(app: AppHandle, seq: u64) {
  let state = app.state::<HudStore>().0.lock().unwrap().clone();
  if state.seq != seq {
    return;
  }
  match state.action {
    HudAction::None => {}
    HudAction::OpenLink => {
      if let Some(url) = &state.target {
        open_external(url);
      }
    }
    HudAction::RevealFile => {
      if let Some(path) = &state.target {
        reveal_file(Path::new(path));
      }
    }
    HudAction::OpenStorageSettings => {
      let _ = open_settings_window(&app, "storage");
    }
    HudAction::OpenScreenRecordingSettings => {
      open_external("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture");
    }
    HudAction::OpenKeyboardSettings => {
      open_external("x-apple.systempreferences:com.apple.Keyboard-Settings.extension");
    }
  }
  hide_hud(&app, seq);
}

fn open_external(target: &str) {
  #[cfg(target_os = "macos")]
  let result = Command::new("open").arg(target).status();
  #[cfg(target_os = "windows")]
  let result = Command::new("cmd").args(["/C", "start", "", target]).status();
  #[cfg(target_os = "linux")]
  let result = Command::new("xdg-open").arg(target).status();
  if let Err(error) = result {
    println!("[open] failed to open {target}: {error}");
  }
}

fn reveal_file(path: &Path) {
  #[cfg(target_os = "macos")]
  {
    let _ = Command::new("open").arg("-R").arg(path).status();
  }
  #[cfg(not(target_os = "macos"))]
  if let Some(parent) = path.parent() {
    open_external(&parent.display().to_string());
  }
}

#[tauri::command]
fn open_system_settings(pane: String) {
  match pane.as_str() {
    "screen_recording" => {
      open_external("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    }
    "keyboard" => open_external("x-apple.systempreferences:com.apple.Keyboard-Settings.extension"),
    _ => {}
  }
}

/// The shortcut recorder pauses the global hotkeys so pressing them records instead of capturing.
#[tauri::command]
fn set_hotkeys_paused(app: AppHandle, paused: bool) -> Result<(), String> {
  let mut manager = app.global_shortcut_manager();
  let _ = manager.unregister_all();
  if paused {
    return Ok(());
  }
  register_shortcuts(&app).map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn open_pictures_folder() {
  if let Some(dir) = pictures_dir() {
    let _ = fs::create_dir_all(&dir);
    open_external(&dir.display().to_string());
  }
}

// ---------------------------------------------------------------------------------------------
// Settings window

fn open_settings_window(app: &AppHandle, tab: &str) -> Result<()> {
  if let Some(window) = app.get_window("settings") {
    let _ = window.emit("settings-tab", tab);
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    return Ok(());
  }
  let mut builder = tauri::WindowBuilder::new(app, "settings", window_url("settings.html"))
    .initialization_script(&format!("window.__SETTINGS_TAB__ = {tab:?};{}", demo_script()))
    .title("Screenshotr")
    .inner_size(600.0, 560.0)
    .resizable(false)
    .minimizable(false)
    .maximizable(false)
    .visible(false);
  #[cfg(target_os = "macos")]
  {
    builder = builder.title_bar_style(TitleBarStyle::Overlay).hidden_title(true);
  }
  let window = builder.build()?;
  let _ = window.center();
  #[cfg(target_os = "macos")]
  macos_document_window_animation(&window);
  // The page shows itself after its first render to avoid a blank flash; this covers a page that fails.
  let fallback = window.clone();
  thread::spawn(move || {
    thread::sleep(Duration::from_millis(1500));
    if !fallback.is_visible().unwrap_or(true) {
      let _ = fallback.show();
      let _ = fallback.set_focus();
    }
  });
  Ok(())
}

// ---------------------------------------------------------------------------------------------
// Platform capture

struct CaptureResult {
  path: Option<PathBuf>,
  bytes: Vec<u8>,
}

fn capture_screenshot(mode: CaptureMode) -> Result<Option<CaptureResult>> {
  #[cfg(target_os = "macos")]
  {
    // Without Screen Recording access, area capture still shows the crosshair but never writes a file.
    if !macos_screen_capture_allowed() {
      return Err(Notice::screen_recording().into());
    }

    println!("[capture][macos] running screencapture");
    let path = temp_png_path();
    let mut cmd = Command::new("screencapture");
    // -x: Screenshotr plays its own capture sound instead of the system's.
    cmd.arg("-x").arg("-t").arg("png");
    if matches!(mode, CaptureMode::Area) {
      cmd.arg("-i");
    }
    cmd.arg(&path);
    let started = std::time::Instant::now();
    let output = cmd.output().context("failed to run screencapture")?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !path.exists() {
      if matches!(mode, CaptureMode::Area) && stderr.is_empty() {
        // When the system Cmd+Shift+4 shortcut also fires, its capture UI takes over and ours exits
        // at once. No one cancels a selection this fast, so report the conflict instead.
        if started.elapsed() < Duration::from_millis(750) {
          return Err(Notice::shortcut_conflict().into());
        }
        // Pressing Esc during interactive capture leaves no file and prints nothing.
        return Ok(None);
      }
      if stderr.contains("could not create image") {
        return Err(Notice::screen_recording().into());
      }
      return Err(anyhow!(
        "screencapture exited with {} and wrote no file{}",
        output.status,
        if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
      ));
    }
    if !output.status.success() {
      return Err(anyhow!("screencapture exited with status {}", output.status));
    }
    let bytes = fs::read(&path).with_context(|| format!("failed reading screenshot at {}", path.display()))?;
    return Ok(Some(CaptureResult { path: Some(path), bytes }));
  }

  #[cfg(target_os = "windows")]
  {
    match mode {
      CaptureMode::Full => {
        println!("[capture][windows] fullscreen capture");
        let path = temp_png_path();
        capture_fullscreen_windows(&path)?;
        let bytes = fs::read(&path).context("failed reading screenshot")?;
        return Ok(Some(CaptureResult { path: Some(path), bytes }));
      }
      CaptureMode::Area => {
        println!("[capture][windows] area capture");
        let bytes = capture_area_windows()?;
        return Ok(bytes.map(|bytes| CaptureResult { path: None, bytes }));
      }
    }
  }

  #[cfg(target_os = "linux")]
  {
    let path = temp_png_path();
    if matches!(mode, CaptureMode::Area) {
      println!("[capture][linux] area capture");
      if let Ok(status) = Command::new("gnome-screenshot").arg("-a").arg("-f").arg(&path).status() {
        if status.success() {
          // gnome-screenshot exits successfully without writing a file when the selection is cancelled.
          if !path.exists() {
            return Ok(None);
          }
          let bytes = fs::read(&path).context("failed reading screenshot")?;
          return Ok(Some(CaptureResult { path: Some(path), bytes }));
        }
      }
      if let Ok(status) = Command::new("import").arg(&path).status() {
        if status.success() {
          let bytes = fs::read(&path).context("failed reading screenshot")?;
          return Ok(Some(CaptureResult { path: Some(path), bytes }));
        }
      }
      return Err(anyhow!("no area screenshot tool found (gnome-screenshot or import)"));
    }

    println!("[capture][linux] fullscreen capture");
    if let Ok(status) = Command::new("gnome-screenshot").arg("-f").arg(&path).status() {
      if status.success() {
        let bytes = fs::read(&path).context("failed reading screenshot")?;
        return Ok(Some(CaptureResult { path: Some(path), bytes }));
      }
    }
    if let Ok(status) = Command::new("scrot").arg(&path).status() {
      if status.success() {
        let bytes = fs::read(&path).context("failed reading screenshot")?;
        return Ok(Some(CaptureResult { path: Some(path), bytes }));
      }
    }
    return Err(anyhow!("no fullscreen screenshot tool found (gnome-screenshot or scrot)"));
  }

  #[allow(unreachable_code)]
  Err(anyhow!("unsupported platform"))
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
  fn CGPreflightScreenCaptureAccess() -> bool;
  fn CGRequestScreenCaptureAccess() -> bool;
}

// IDs from com.apple.symbolichotkeys: 28 = Cmd+Shift+3 (save screen), 30 = Cmd+Shift+4 (save selection).
#[cfg(target_os = "macos")]
const MACOS_SCREENSHOT_SHORTCUTS: [(&str, &str); 2] = [("cmd+shift+3", "28"), ("cmd+shift+4", "30")];
#[cfg(target_os = "macos")]
const SYMBOLIC_HOTKEYS_DOMAIN: &str = "com.apple.symbolichotkeys";

/// Turns off the built-in Screenshot shortcuts that our hotkeys collide with, and turns back on any
/// we turned off earlier that no longer collide. Which ones we changed is kept on disk so they can be
/// restored on quit, including after a crash (the next clean quit restores them).
#[cfg(target_os = "macos")]
fn macos_sync_system_shortcuts(app: &AppHandle, config: &AppConfig) -> Result<()> {
  let normalize = |hotkey: &str| {
    hotkey
      .to_ascii_lowercase()
      .replace(' ', "")
      .replace("command", "cmd")
      .replace("super", "cmd")
  };
  let (full, area) = current_hotkeys(config);
  let configured = [normalize(full), normalize(area)];
  let wanted: Vec<&str> = MACOS_SCREENSHOT_SHORTCUTS
    .iter()
    .filter(|(combo, _)| configured.iter().any(|hotkey| hotkey == combo))
    .map(|(_, id)| *id)
    .collect();
  macos_apply_system_shortcuts(app, &wanted)
}

/// Turns Ctrl+C, `kill` and hangup into a normal quit so the system shortcuts are restored.
#[cfg(target_os = "macos")]
fn exit_on_termination_signal(app: &AppHandle) {
  use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

  let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) {
    Ok(signals) => signals,
    Err(error) => {
      println!("[startup] failed to install signal handler: {error}");
      return;
    }
  };
  let app = app.clone();
  thread::spawn(move || {
    if let Some(signal) = signals.forever().next() {
      println!("[shutdown] received signal {signal}");
      quit(&app);
    }
  });
}

#[cfg(target_os = "macos")]
fn macos_restore_system_shortcuts(app: &AppHandle) -> Result<()> {
  macos_apply_system_shortcuts(app, &[])
}

#[cfg(target_os = "macos")]
fn macos_apply_system_shortcuts(app: &AppHandle, wanted: &[&str]) -> Result<()> {
  use std::collections::BTreeMap;

  // Maps each shortcut ID we disabled to whether it had an entry beforehand.
  let state_path = config_path(app)?.with_file_name("disabled_system_shortcuts.json");
  let mut owned: BTreeMap<String, bool> = fs::read_to_string(&state_path)
    .ok()
    .and_then(|contents| serde_json::from_str(&contents).ok())
    .unwrap_or_default();

  let mut prefs = macos_read_symbolic_hotkeys()?;
  if !prefs.contains_key("AppleSymbolicHotKeys") {
    prefs.insert(
      "AppleSymbolicHotKeys".to_string(),
      plist::Value::Dictionary(plist::Dictionary::new()),
    );
  }
  let hotkeys = prefs
    .get_mut("AppleSymbolicHotKeys")
    .and_then(|value| value.as_dictionary_mut())
    .ok_or_else(|| anyhow!("AppleSymbolicHotKeys is not a dictionary"))?;
  let mut changed = false;

  for id in wanted {
    if owned.contains_key(*id) {
      continue;
    }
    // A missing entry means the shortcut was never changed, so it's on by default.
    let had_entry = hotkeys.contains_key(*id);
    let enabled = hotkeys
      .get(*id)
      .and_then(|entry| entry.as_dictionary())
      .and_then(|entry| entry.get("enabled"))
      .and_then(|enabled| enabled.as_boolean())
      .unwrap_or(true);
    if !enabled {
      continue;
    }
    match hotkeys.get_mut(*id).and_then(|entry| entry.as_dictionary_mut()) {
      Some(entry) => {
        entry.insert("enabled".to_string(), plist::Value::Boolean(false));
      }
      None => {
        let mut entry = plist::Dictionary::new();
        entry.insert("enabled".to_string(), plist::Value::Boolean(false));
        hotkeys.insert(id.to_string(), plist::Value::Dictionary(entry));
      }
    }
    println!("[hotkeys] disabled system screenshot shortcut {id}");
    owned.insert(id.to_string(), had_entry);
    changed = true;
  }

  let to_restore: Vec<String> = owned.keys().filter(|id| !wanted.contains(&id.as_str())).cloned().collect();
  for id in to_restore {
    let had_entry = owned.remove(&id).unwrap_or(false);
    if had_entry {
      if let Some(entry) = hotkeys.get_mut(&id).and_then(|entry| entry.as_dictionary_mut()) {
        entry.insert("enabled".to_string(), plist::Value::Boolean(true));
      }
    } else {
      hotkeys.remove(&id);
    }
    println!("[hotkeys] restored system screenshot shortcut {id}");
    changed = true;
  }

  if changed {
    macos_write_symbolic_hotkeys(&prefs)?;
  }
  if owned.is_empty() {
    let _ = fs::remove_file(&state_path);
  } else {
    fs::write(&state_path, serde_json::to_string_pretty(&owned)?).context("failed to save shortcut state")?;
  }
  Ok(())
}

#[cfg(target_os = "macos")]
fn macos_read_symbolic_hotkeys() -> Result<plist::Dictionary> {
  let output = Command::new("defaults")
    .args(["export", SYMBOLIC_HOTKEYS_DOMAIN, "-"])
    .output()
    .context("failed to run defaults export")?;
  if !output.status.success() {
    return Err(anyhow!("defaults export exited with {}", output.status));
  }
  plist::from_bytes(&output.stdout).context("failed to parse symbolic hotkeys")
}

/// Writes the preferences through cfprefsd and applies them without a logout.
#[cfg(target_os = "macos")]
fn macos_write_symbolic_hotkeys(prefs: &plist::Dictionary) -> Result<()> {
  use std::io::Write;
  use std::process::Stdio;

  let mut xml = Vec::new();
  plist::to_writer_xml(&mut xml, prefs).context("failed to encode symbolic hotkeys")?;
  let mut child = Command::new("defaults")
    .args(["import", SYMBOLIC_HOTKEYS_DOMAIN, "-"])
    .stdin(Stdio::piped())
    .spawn()
    .context("failed to run defaults import")?;
  child
    .stdin
    .take()
    .ok_or_else(|| anyhow!("defaults import has no stdin"))?
    .write_all(&xml)
    .context("failed to write symbolic hotkeys")?;
  let status = child.wait().context("failed to wait for defaults import")?;
  if !status.success() {
    return Err(anyhow!("defaults import exited with {status}"));
  }
  let _ = Command::new("/System/Library/PrivateFrameworks/SystemAdministration.framework/Resources/activateSettings")
    .arg("-u")
    .status();
  Ok(())
}

/// Returns whether this app may record the screen. Shows the system prompt at most once per launch;
/// after that a missing permission only produces the overlay notice.
#[cfg(target_os = "macos")]
fn macos_screen_capture_allowed() -> bool {
  use std::sync::atomic::{AtomicBool, Ordering};

  static REQUESTED: AtomicBool = AtomicBool::new(false);
  if unsafe { CGPreflightScreenCaptureAccess() } {
    return true;
  }
  if REQUESTED.swap(true, Ordering::SeqCst) {
    return false;
  }
  unsafe { CGRequestScreenCaptureAccess() }
}

fn capture_fullscreen_windows(path: &Path) -> Result<()> {
  println!("[capture][windows] running PowerShell capture");
  let script = format!(
    "Add-Type -AssemblyName System.Windows.Forms; \
     Add-Type -AssemblyName System.Drawing; \
     $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds; \
     $bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height; \
     $graphics = [System.Drawing.Graphics]::FromImage($bmp); \
     $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size); \
     $bmp.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png); \
     $graphics.Dispose(); \
     $bmp.Dispose();",
    path.display()
  );

  let status = Command::new("powershell")
    .args(["-NoProfile", "-Command", &script])
    .status()
    .context("failed to run PowerShell screenshot script")?;

  if !status.success() {
    return Err(anyhow!("PowerShell screenshot script failed"));
  }

  Ok(())
}

fn capture_area_windows() -> Result<Option<Vec<u8>>> {
  let mut clipboard = Clipboard::new().context("failed to access clipboard")?;
  // Remember any image already on the clipboard so it isn't mistaken for the new snip.
  let baseline = clipboard
    .get_image()
    .ok()
    .map(|image| (image.width, image.height, image.bytes.into_owned()));

  println!("[capture][windows] invoking snip UI");
  let _ = Command::new("cmd")
    .args(["/C", "start", "", "ms-screenclip:"])
    .status();

  // The snip UI gives no signal on cancel, so give the user up to 60s to select.
  for _ in 0..240 {
    if let Ok(image) = clipboard.get_image() {
      let unchanged = baseline.as_ref().is_some_and(|(width, height, bytes)| {
        *width == image.width && *height == image.height && bytes.as_slice() == image.bytes.as_ref()
      });
      if unchanged {
        thread::sleep(Duration::from_millis(250));
        continue;
      }

      let mut bytes = image.bytes.into_owned();
      for chunk in bytes.chunks_mut(4) {
        chunk.swap(0, 2);
      }

      let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(
        image.width as u32,
        image.height as u32,
        bytes,
      )
      .ok_or_else(|| anyhow!("clipboard image buffer malformed"))?;

      let mut png_bytes: Vec<u8> = Vec::new();
      let mut cursor = Cursor::new(&mut png_bytes);
      buffer
        .write_to(&mut cursor, image::ImageOutputFormat::Png)
        .context("failed to encode clipboard image")?;

      return Ok(Some(png_bytes));
    }

    thread::sleep(Duration::from_millis(250));
  }

  println!("[capture][windows] no new image on clipboard, assuming snip was cancelled");
  Ok(None)
}

// ---------------------------------------------------------------------------------------------
// Destinations

fn active_destination(config: &AppConfig) -> Option<Destination> {
  config.destinations.iter().find(|destination| destination.id == config.active_destination).cloned()
}

fn is_configured(config: &AppConfig) -> bool {
  active_destination(config).is_some()
}

fn destination_secrets_account(id: &str) -> String {
  format!("destination:{id}")
}

/// Splits credentials out of a destination so they can go to the keychain.
fn strip_destination_secrets(destination: &Destination) -> (Destination, serde_json::Map<String, serde_json::Value>) {
  let mut secrets = serde_json::Map::new();
  let Ok(serde_json::Value::Object(mut fields)) = serde_json::to_value(destination) else {
    return (destination.clone(), secrets);
  };
  for name in destinations::SECRET_FIELDS {
    if let Some(value) = fields.remove(*name) {
      let empty = match &value {
        serde_json::Value::String(text) => text.is_empty(),
        serde_json::Value::Object(map) => map.is_empty(),
        serde_json::Value::Null => true,
        _ => false,
      };
      if !empty {
        secrets.insert(name.to_string(), value);
      }
    }
  }
  let public = serde_json::from_value(serde_json::Value::Object(fields)).unwrap_or_else(|_| destination.clone());
  (public, secrets)
}

fn load_destination_secrets(destination: &mut Destination) {
  let Some(stored) = read_keyring_secret(&destination_secrets_account(&destination.id)) else {
    return;
  };
  let (Ok(serde_json::Value::Object(secrets)), Ok(serde_json::Value::Object(mut fields))) =
    (serde_json::from_str::<serde_json::Value>(&stored), serde_json::to_value(&*destination))
  else {
    return;
  };
  fields.extend(secrets);
  if let Ok(merged) = serde_json::from_value(serde_json::Value::Object(fields)) {
    *destination = merged;
  }
}

/// Writes config.json without any credentials, and each destination's credentials to the keychain.
fn persist_config(app: &AppHandle, config: &AppConfig) -> Result<()> {
  let mut to_save = config.clone();
  to_save.r2_access_key_id.clear();
  to_save.r2_secret_access_key.clear();
  to_save.destinations = config
    .destinations
    .iter()
    .map(|destination| {
      let (public, secrets) = strip_destination_secrets(destination);
      let value = if secrets.is_empty() { String::new() } else { serde_json::Value::Object(secrets).to_string() };
      if let Err(error) = write_keyring_secret(&destination_secrets_account(&destination.id), &value) {
        println!("[config] {error}");
      }
      public
    })
    .collect();
  save_config(app, &to_save)
}

/// Turns the original single R2 setup into a saved destination, once.
fn migrate_legacy_storage(config: &mut AppConfig) -> bool {
  if !config.destinations.is_empty()
    || config.r2_access_key_id.trim().is_empty()
    || config.r2_secret_access_key.trim().is_empty()
    || config.r2_endpoint.trim().is_empty()
  {
    return false;
  }
  let is_r2 = config.r2_endpoint.contains("r2.cloudflarestorage.com");
  let destination = Destination {
    id: random_suffix(10),
    name: if is_r2 { "Cloudflare R2" } else { "S3 storage" }.to_string(),
    kind: destinations::Kind::S3(destinations::S3 {
      provider: if is_r2 { "r2" } else { "other" }.to_string(),
      endpoint: config.r2_endpoint.trim().to_string(),
      region: if is_r2 { "auto".to_string() } else { String::new() },
      bucket: config.r2_bucket.trim().to_string(),
      access_key_id: config.r2_access_key_id.trim().to_string(),
      secret_access_key: config.r2_secret_access_key.trim().to_string(),
      public_base_url: config.public_base_url.trim().to_string(),
      ..destinations::S3::default()
    }),
  };
  println!("[config] moved the existing storage settings into destination {}", destination.id);
  config.active_destination = destination.id.clone();
  config.destinations.push(destination);
  config.r2_access_key_id.clear();
  config.r2_secret_access_key.clear();
  for account in [KEYRING_R2_ACCESS_KEY_ID, KEYRING_R2_SECRET_ACCESS_KEY] {
    let _ = write_keyring_secret(account, "");
  }
  true
}

/// The clipboard text for a link in the chosen format, and the overlay title that names it.
fn formatted_link(url: &str, format: &str) -> (String, &'static str) {
  match format {
    "markdown" => (format!("![Screenshot]({url})"), "Markdown copied"),
    "html" => (format!("<img src=\"{url}\" alt=\"Screenshot\">"), "HTML copied"),
    _ => (url.to_string(), "Link copied"),
  }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
  let mut clipboard = Clipboard::new().context("failed to access clipboard")?;
  clipboard.set_text(text.to_string()).context("failed to set clipboard")?;
  Ok(())
}

fn temp_png_path() -> PathBuf {
  let stamp = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_else(|_| Duration::from_secs(0))
    .as_millis();
  env::temp_dir().join(format!("screenshot-{stamp}.png"))
}

fn object_key(path: &Option<PathBuf>) -> String {
  let stamp = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_else(|_| Duration::from_secs(0))
    .as_millis();
  let rand = random_suffix(8);

  if let Some(path) = path {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
      if let Some((stem, ext)) = name.rsplit_once('.') {
        return format!("{stem}-{stamp}-{rand}.{ext}");
      }
      return format!("{name}-{stamp}-{rand}");
    }
  }

  format!("screenshot-{stamp}-{rand}.png")
}

fn default_hotkeys() -> (String, String) {
  #[cfg(target_os = "macos")]
  {
    return ("Cmd+Shift+3".to_string(), "Cmd+Shift+4".to_string());
  }
  #[cfg(target_os = "windows")]
  {
    return ("PrintScreen".to_string(), "Win+Shift+S".to_string());
  }
  #[cfg(target_os = "linux")]
  {
    return ("PrintScreen".to_string(), "Shift+PrintScreen".to_string());
  }
  #[allow(unreachable_code)]
  (String::new(), String::new())
}

fn random_suffix(len: usize) -> String {
  rand::thread_rng()
    .sample_iter(&Alphanumeric)
    .take(len)
    .map(char::from)
    .collect()
}

fn current_hotkeys(config: &AppConfig) -> (&str, &str) {
  (config.hotkey_full.as_str(), config.hotkey_area.as_str())
}

fn open_welcome_window(app: &AppHandle) -> Result<()> {
  if let Some(window) = app.get_window("welcome") {
    let _ = window.show();
    let _ = window.set_focus();
    return Ok(());
  }
  let mut builder = tauri::WindowBuilder::new(app, "welcome", window_url("welcome.html"))
    .title("Welcome to Screenshotr")
    .inner_size(520.0, 530.0)
    .resizable(false)
    .minimizable(false)
    .maximizable(false)
    .visible(false);
  #[cfg(target_os = "macos")]
  {
    builder = builder.title_bar_style(TitleBarStyle::Overlay).hidden_title(true);
  }
  let window = builder.build()?;
  let _ = window.center();
  #[cfg(target_os = "macos")]
  macos_document_window_animation(&window);
  // The page shows itself after its first render; this covers a page that fails.
  let fallback = window.clone();
  thread::spawn(move || {
    thread::sleep(Duration::from_millis(1500));
    if !fallback.is_visible().unwrap_or(true) {
      let _ = fallback.show();
      let _ = fallback.set_focus();
    }
  });
  Ok(())
}

/// Asks macOS for Screen Recording access (it prompts once) and opens the pane to turn it on.
#[tauri::command]
fn request_screen_recording() {
  #[cfg(target_os = "macos")]
  {
    if unsafe { CGPreflightScreenCaptureAccess() } {
      return;
    }
    unsafe { CGRequestScreenCaptureAccess() };
    open_external("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture");
  }
}

/// macOS applies a new Screen Recording grant only after the app restarts.
#[tauri::command]
fn relaunch_app(app: AppHandle) {
  release_hotkeys(&app);
  tauri::api::process::restart(&app.env());
}

/// Resizes the calling window to `height` points. On macOS this keeps the top edge in place and,
/// once the window is on screen, animates like a native preferences window switching panes.
#[tauri::command]
fn fit_window_height(window: Window, height: f64) {
  #[cfg(target_os = "macos")]
  {
    let target = window.clone();
    let _ = window.run_on_main_thread(move || {
      let Ok(ns_window) = target.ns_window() else {
        return;
      };
      unsafe {
        use cocoa::base::{id, NO, YES};
        use cocoa::foundation::NSRect;
        let ns_window = ns_window as id;
        let mut frame: NSRect = msg_send![ns_window, frame];
        // Full-size content view: the frame height is the content height. AppKit's origin is the
        // bottom-left corner, so move it down by the growth to keep the title bar still.
        let delta = height - frame.size.height;
        if delta.abs() < 0.5 {
          return;
        }
        frame.origin.y -= delta;
        frame.size.height = height;
        let visible: bool = msg_send![ns_window, isVisible];
        if !visible {
          let _: () = msg_send![ns_window, setFrame: frame display: NO];
          return;
        }
        // The animator proxy runs on Core Animation and doesn't block the main thread, unlike
        // setFrame:display:animate:, so the page's own transitions keep playing while it resizes.
        let curve: id = objc::Message::send_message(
          class!(CAMediaTimingFunction),
          objc::runtime::Sel::register("functionWithControlPoints::::"),
          (0.25f32, 0.9f32, 0.3f32, 1.0f32),
        )
        .unwrap_or(std::ptr::null_mut());
        let _: () = msg_send![class!(NSAnimationContext), beginGrouping];
        let context: id = msg_send![class!(NSAnimationContext), currentContext];
        let _: () = msg_send![context, setDuration: 0.38f64];
        if !curve.is_null() {
          let _: () = msg_send![context, setTimingFunction: curve];
        }
        let animator: id = msg_send![ns_window, animator];
        let _: () = msg_send![animator, setFrame: frame display: YES];
        let _: () = msg_send![class!(NSAnimationContext), endGrouping];
      }
    });
  }
  #[cfg(not(target_os = "macos"))]
  {
    let width = window.inner_size().ok().and_then(|size| window.scale_factor().ok().map(|scale| size.to_logical::<f64>(scale).width));
    let _ = window.set_size(tauri::LogicalSize::new(width.unwrap_or(600.0), height));
  }
}

#[tauri::command]
fn open_settings(app: AppHandle, tab: String) -> Result<(), String> {
  open_settings_window(&app, &tab).map_err(|error| error.to_string())
}

#[tauri::command]
fn finish_onboarding(app: AppHandle, state: State<ConfigState>) -> Result<(), String> {
  let config = {
    let mut config = state.0.lock().unwrap();
    config.onboarded = true;
    config.clone()
  };
  persist_config(&app, &config).map_err(|error| error.to_string())?;
  if let Some(window) = app.get_window("welcome") {
    let _ = window.close();
  }
  Ok(())
}

/// Debug builds only: `SCREENSHOTR_DEMO=settings` or `=hud` plays the app's motion on a loop, so
/// transitions can be recorded and reviewed in the real windows without capturing or uploading.
#[cfg(debug_assertions)]
fn run_demo(app: &AppHandle) {
  match env::var("SCREENSHOTR_DEMO").as_deref() {
    Ok("settings") => {
      let _ = open_settings_window(app, "general");
      // WebKit pauses animation in covered windows, so keep the demo in front while it plays.
      if let Some(window) = app.get_window("settings") {
        let _ = window.set_always_on_top(true);
      }
    }
    Ok("hud") => {
      let app = app.clone();
      thread::spawn(move || {
        let thumb = thumbnail_data_url(include_bytes!("../icons/icon.png")).ok();
        thread::sleep(Duration::from_millis(1500));
        loop {
          tray_uploading(&app);
          show_hud(
            &app,
            HudState {
              phase: "uploading".to_string(),
              title: "Uploading".to_string(),
              message: "to i.aahmed.ca".to_string(),
              thumb: thumb.clone(),
              ..HudState::default()
            },
          );
          thread::sleep(Duration::from_millis(1400));
          tray_flash(&app, TRAY_DONE, Duration::from_millis(1800));
          show_hud(
            &app,
            HudState {
              phase: "done".to_string(),
              title: "Link copied".to_string(),
              message: "i.aahmed.ca/screenshot-1792634551000-k2Jd8fQa.png".to_string(),
              thumb: thumb.clone(),
              action: HudAction::OpenLink,
              ..HudState::default()
            },
          );
          thread::sleep(Duration::from_millis(5000));
        }
      });
    }
    _ => {}
  }
}

/// The scripted Settings tour for `SCREENSHOTR_DEMO=settings`; empty otherwise.
fn demo_script() -> &'static str {
  #[cfg(debug_assertions)]
  if env::var("SCREENSHOTR_DEMO").as_deref() == Ok("settings") {
    return r#"
      window.addEventListener("load", () => {
        const steps = [
          [1500, () => selectTab("storage")],
          [1600, () => openPicker()],
          [1600, () => document.querySelector('.tile[data-kind="s3"]').click()],
          [1500, () => { const s = document.getElementById("field-provider"); s.value = "minio"; s.dispatchEvent(new Event("change")); }],
          [1500, () => document.querySelector(".back").click()],
          [1500, () => selectTab("about")],
          [1600, () => selectTab("general")],
        ];
        let at = 0;
        for (const [wait, step] of steps) { at += wait; setTimeout(step, at); }
      });
    "#;
  }
  ""
}

fn open_history_window(app: &AppHandle) -> Result<()> {
  if let Some(window) = app.get_window("history") {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    return Ok(());
  }
  let mut builder = tauri::WindowBuilder::new(app, "history", window_url("history.html"))
    .title("Upload History")
    .inner_size(760.0, 560.0)
    .min_inner_size(520.0, 360.0)
    .visible(false);
  #[cfg(target_os = "macos")]
  {
    builder = builder.title_bar_style(TitleBarStyle::Overlay).hidden_title(true);
  }
  let window = builder.build()?;
  let _ = window.center();
  #[cfg(target_os = "macos")]
  macos_document_window_animation(&window);
  let fallback = window.clone();
  thread::spawn(move || {
    thread::sleep(Duration::from_millis(1500));
    if !fallback.is_visible().unwrap_or(true) {
      let _ = fallback.show();
      let _ = fallback.set_focus();
    }
  });
  Ok(())
}

#[derive(Serialize)]
struct HistoryItem {
  #[serde(flatten)]
  upload: Upload,
  has_thumbnail: bool,
  /// Whether "Delete from server" can be done from here.
  deletable: bool,
}

#[tauri::command]
fn history_list(app: AppHandle, state: State<HistoryState>, config: State<ConfigState>) -> Vec<HistoryItem> {
  let dir = thumbnails_dir(&app).ok();
  let config = config.0.lock().unwrap().clone();
  state
    .0
    .lock()
    .unwrap()
    .iter()
    .cloned()
    .map(|upload| {
      let destination = config.destinations.iter().find(|destination| destination.id == upload.destination_id);
      let deletable = match destination.map(|destination| &destination.kind) {
        Some(destinations::Kind::S3(_)) => upload.remote_key.is_some(),
        Some(destinations::Kind::Imgur(_)) => upload.deletion_url.is_some(),
        _ => upload.deletion_url.is_some(),
      };
      let has_thumbnail = dir.as_ref().map(|dir| dir.join(format!("{}.png", upload.id)).exists()).unwrap_or(false);
      HistoryItem { upload, has_thumbnail, deletable }
    })
    .collect()
}

#[tauri::command]
fn history_thumbnail(app: AppHandle, id: String) -> Option<String> {
  let bytes = fs::read(thumbnails_dir(&app).ok()?.join(format!("{id}.png"))).ok()?;
  Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[tauri::command]
fn history_copy(app: AppHandle, ids: Vec<String>) -> Result<String, String> {
  let format = app.state::<ConfigState>().0.lock().unwrap().link_format.clone();
  let history = app.state::<HistoryState>().0.lock().unwrap().clone();
  let texts: Vec<String> =
    ids.iter().filter_map(|id| history.iter().find(|upload| &upload.id == id)).map(|upload| formatted_link(&upload.url, &format).0).collect();
  copy_to_clipboard(&texts.join("\n")).map_err(|error| error.to_string())?;
  Ok(if texts.len() == 1 { "Copied".to_string() } else { format!("Copied {} links", texts.len()) })
}

#[tauri::command]
fn history_open(app: AppHandle, id: String) {
  let url = app.state::<HistoryState>().0.lock().unwrap().iter().find(|upload| upload.id == id).map(|upload| upload.url.clone());
  if let Some(url) = url {
    open_external(&url);
  }
}

/// Forgets uploads, and with `from_server` also deletes them where the destination allows.
/// ShareX uploaders give a deletion page instead of an API, so that opens in the browser.
#[tauri::command]
async fn history_remove(app: AppHandle, ids: Vec<String>, from_server: bool) -> Result<String, String> {
  let history = app.state::<HistoryState>().0.lock().unwrap().clone();
  let config = app.state::<ConfigState>().0.lock().unwrap().clone();
  let mut deleted = 0;
  for upload in history.iter().filter(|upload| ids.contains(&upload.id)) {
    if !from_server {
      continue;
    }
    let destination = config.destinations.iter().find(|destination| destination.id == upload.destination_id);
    let done = match destination {
      Some(destination) => destinations::delete(destination, upload.remote_key.as_deref(), upload.deletion_url.as_deref())
        .await
        .map_err(|error| format!("{}: {error}", upload.name))?,
      None => false,
    };
    if done {
      deleted += 1;
    } else if let Some(url) = &upload.deletion_url {
      open_external(url);
    }
  }
  {
    let state = app.state::<HistoryState>();
    state.0.lock().unwrap().retain(|upload| !ids.contains(&upload.id));
  }
  if let Ok(dir) = thumbnails_dir(&app) {
    for id in &ids {
      let _ = fs::remove_file(dir.join(format!("{id}.png")));
    }
  }
  save_history(&app);
  update_tray(&app);
  let _ = app.emit_all("history-changed", ());
  Ok(match (from_server, deleted) {
    (true, 0) => "Removed from history".to_string(),
    (true, 1) => "Deleted from the server".to_string(),
    (true, count) => format!("Deleted {count} from the server"),
    _ => "Removed from history".to_string(),
  })
}

#[tauri::command]
fn upload_files(app: AppHandle, paths: Vec<String>) {
  upload_paths(&app, paths.into_iter().map(PathBuf::from).collect());
}

#[tauri::command]
fn upload_chooser(app: AppHandle) {
  choose_files(&app);
}

// ---------------------------------------------------------------------------------------------
// Updates

#[derive(Serialize)]
struct UpdateCheck {
  available: bool,
  version: String,
  notes: String,
}

/// Checks once shortly after launch and then daily, quietly: nothing is shown unless there's a
/// newer version, and then only a menu item and a note in Settings.
fn start_update_checks(app: &AppHandle) {
  if cfg!(debug_assertions) {
    return;
  }
  let app = app.clone();
  tauri::async_runtime::spawn(async move {
    tokio_sleep(Duration::from_secs(20)).await;
    loop {
      let enabled = app.state::<ConfigState>().0.lock().unwrap().check_updates;
      if enabled {
        if let Err(error) = check_for_update(app.clone()).await {
          println!("[update] check failed: {error}");
        }
      }
      tokio_sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
  });
}

async fn tokio_sleep(duration: Duration) {
  let _ = tauri::async_runtime::spawn_blocking(move || thread::sleep(duration)).await;
}

#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<UpdateCheck, String> {
  let update = app.updater().check().await.map_err(|error| describe_update_error(&error.to_string()))?;
  let available = update.is_update_available();
  let version = update.latest_version().to_string();
  *app.state::<UpdateState>().0.lock().unwrap() = available.then(|| version.clone());
  update_tray(&app);
  if available {
    let _ = app.emit_all("update-available", &version);
  }
  Ok(UpdateCheck { available, version, notes: update.body().cloned().unwrap_or_default() })
}

/// Downloads the signed update, replaces the app and relaunches it.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
  let update = app.updater().check().await.map_err(|error| describe_update_error(&error.to_string()))?;
  if !update.is_update_available() {
    return Err("Screenshotr is already up to date.".to_string());
  }
  update.download_and_install().await.map_err(|error| describe_update_error(&error.to_string()))?;
  release_hotkeys(&app);
  tauri::api::process::restart(&app.env());
  Ok(())
}

fn describe_update_error(error: &str) -> String {
  if error.contains("404") || error.contains("Not Found") {
    "No update feed is published for this build yet.".to_string()
  } else if error.contains("signature") {
    "The update's signature didn't match, so it wasn't installed.".to_string()
  } else if error.contains("dns") || error.contains("connect") || error.contains("timed out") {
    "Couldn't reach the update server.".to_string()
  } else {
    format!("Couldn't check for updates: {error}")
  }
}

const CAPTURE_SOUND: &[u8] = include_bytes!("../sounds/capture.wav");

/// Plays Screenshotr's own capture sound, made by sounds/make_capture_sound.py. Like system
/// sounds, it stays quiet when "Play user interface sound effects" is off in System Settings.
fn play_capture_sound(app: &AppHandle) {
  #[cfg(target_os = "macos")]
  {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // One NSSound for the life of the app; only touched on the main thread.
    static SOUND: AtomicUsize = AtomicUsize::new(0);
    let _ = app.run_on_main_thread(|| unsafe {
      use cocoa::base::{id, nil};
      let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
      let key: id = msg_send![class!(NSString), stringWithUTF8String: "com.apple.sound.uiaudio.enabled\0".as_ptr()];
      let setting: id = msg_send![defaults, objectForKey: key];
      if setting != nil {
        let enabled: bool = msg_send![setting, boolValue];
        if !enabled {
          return;
        }
      }
      let mut sound = SOUND.load(Ordering::Relaxed) as id;
      if sound.is_null() {
        let data: id = msg_send![class!(NSData), dataWithBytes: CAPTURE_SOUND.as_ptr() length: CAPTURE_SOUND.len()];
        let created: id = msg_send![class!(NSSound), alloc];
        sound = msg_send![created, initWithData: data];
        if sound.is_null() {
          return;
        }
        SOUND.store(sound as usize, Ordering::Relaxed);
      }
      let _: bool = msg_send![sound, stop];
      let _: bool = msg_send![sound, play];
    });
  }
  #[cfg(not(target_os = "macos"))]
  let _ = app;
}

/// Lets Settings preview the sound.
#[tauri::command]
fn preview_capture_sound(app: AppHandle) {
  play_capture_sound(&app);
}

/// Opens and closes with the system's document-window animation instead of appearing abruptly.
#[cfg(target_os = "macos")]
fn macos_document_window_animation(window: &Window) {
  let target = window.clone();
  let _ = window.run_on_main_thread(move || {
    if let Ok(ns_window) = target.ns_window() {
      // NSWindowAnimationBehaviorDocumentWindow
      unsafe {
        let _: () = msg_send![ns_window as cocoa::base::id, setAnimationBehavior: 3i64];
      }
    }
  });
}

fn window_url(path: &str) -> WindowUrl {
  if cfg!(debug_assertions) {
    if let Ok(port) = env::var("TAURI_DEV_SERVER_PORT") {
      return WindowUrl::External(format!("http://localhost:{port}/{path}").parse().unwrap());
    }
  }
  WindowUrl::App(path.into())
}

fn load_config(app: &AppHandle) -> Result<(AppConfig, bool)> {
  let path = config_path(app)?;
  let mut config = if path.exists() {
    let contents = fs::read_to_string(&path).context("failed to read config")?;
    serde_json::from_str(&contents).context("invalid config JSON")?
  } else {
    AppConfig::default()
  };


  if !path.exists() {
    if let Some(legacy_dir) = legacy_config_dir() {
      let legacy_path = legacy_dir.join("config.json");
      if legacy_path.exists() {
        if let Ok(contents) = fs::read_to_string(&legacy_path) {
          if let Ok(legacy) = serde_json::from_str::<AppConfig>(&contents) {
            config = legacy;
            let _ = save_config(app, &config);
          }
        }
      }
    }
  }

  let has_plaintext = !config.r2_access_key_id.trim().is_empty() || !config.r2_secret_access_key.trim().is_empty();
  if env::var("SKIP_ENV_CONFIG").unwrap_or_default() != "1" {
    load_env_overrides(app, &mut config);
  }
  load_keyring_secrets(&mut config);
  for destination in &mut config.destinations {
    load_destination_secrets(destination);
  }
  if migrate_legacy_storage(&mut config) || (path.exists() && has_plaintext) {
    persist_config(app, &config)?;
  }

  if path.exists() {
    Ok((config, true))
  } else {
    Ok((config, false))
  }
}

fn load_keyring_secrets(config: &mut AppConfig) {
  if config.r2_access_key_id.trim().is_empty() {
    if let Some(value) = read_keyring_secret(KEYRING_R2_ACCESS_KEY_ID) {
      config.r2_access_key_id = value;
    }
  }
  if config.r2_secret_access_key.trim().is_empty() {
    if let Some(value) = read_keyring_secret(KEYRING_R2_SECRET_ACCESS_KEY) {
      config.r2_secret_access_key = value;
    }
  }
}

fn read_keyring_secret(account: &str) -> Option<String> {
  let entry = match KeyringEntry::new(KEYRING_SERVICE, account) {
    Ok(entry) => entry,
    Err(error) => {
      println!("[config] keyring init failed for {account}: {error}");
      return None;
    }
  };
  match entry.get_password() {
    Ok(value) => Some(value),
    Err(KeyringError::NoEntry) => None,
    Err(error) => {
      println!("[config] keyring read failed for {account}: {error}");
      None
    }
  }
}

fn write_keyring_secret(account: &str, value: &str) -> Result<()> {
  let entry = KeyringEntry::new(KEYRING_SERVICE, account)
    .map_err(|error| anyhow!("failed to init keyring entry {account}: {error}"))?;
  let trimmed = value.trim();
  if trimmed.is_empty() {
    match entry.delete_password() {
      Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
      Err(error) => Err(anyhow!("failed to delete keyring secret {account}: {error}")),
    }
  } else {
    entry
      .set_password(trimmed)
      .map_err(|error| anyhow!("failed to store keyring secret {account}: {error}"))
  }
}

fn save_config(app: &AppHandle, config: &AppConfig) -> Result<()> {
  let path = config_path(app)?;
  let contents = serde_json::to_string_pretty(config).context("failed to serialize config")?;
  fs::write(&path, contents).context("failed to write config")?;
  Ok(())
}

fn config_path(app: &AppHandle) -> Result<PathBuf> {
  let dir = app
    .path_resolver()
    .app_config_dir()
    .ok_or_else(|| anyhow!("unable to resolve app config dir"))?;
  fs::create_dir_all(&dir).context("failed to create config dir")?;
  Ok(dir.join("config.json"))
}

fn legacy_config_dir() -> Option<PathBuf> {
  if cfg!(target_os = "macos") {
    if let Ok(home) = env::var("HOME") {
      return Some(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("ca.aahmed.screenshot-util"));
    }
  }
  None
}

fn load_env_overrides(app: &AppHandle, config: &mut AppConfig) {
  let mut candidates: Vec<PathBuf> = Vec::new();
  if let Some(dir) = app.path_resolver().app_config_dir() {
    candidates.push(dir.join(".env"));
  }
  if let Ok(cwd) = env::current_dir() {
    candidates.push(cwd.join(".env"));
  }

  for env_path in candidates {
    if let Ok(contents) = fs::read_to_string(env_path) {
      println!("[config] loaded .env overrides");
      for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
          continue;
        }
        let mut parts = trimmed.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if value.is_empty() {
          continue;
        }
        match key {
          "SCREENSHOT_PUBLIC_BASE_URL" | "PUBLIC_BASE_URL" => config.public_base_url = value.to_string(),
          "R2_ENDPOINT" => config.r2_endpoint = value.to_string(),
          "R2_BUCKET" => config.r2_bucket = value.to_string(),
          "R2_ACCESS_KEY_ID" => config.r2_access_key_id = value.to_string(),
          "R2_SECRET_ACCESS_KEY" => config.r2_secret_access_key = value.to_string(),
          "HOTKEY_FULL" => config.hotkey_full = value.to_string(),
          "HOTKEY_AREA" => config.hotkey_area = value.to_string(),
          "RETAIN_LOCAL_COPY" => {
            let normalized = value.to_ascii_lowercase();
            config.retain_local_copy = matches!(normalized.as_str(), "1" | "true" | "yes");
          }
          _ => {}
        }
      }
      break;
    }
  }
}

/// "i.aahmed.ca" from a full URL.
fn display_host(url: &str) -> String {
  Url::parse(url.trim())
    .ok()
    .and_then(|url| url.host_str().map(str::to_string))
    .unwrap_or_else(|| "your bucket".to_string())
}

/// "i.aahmed.ca/screens…k2Jd8fQa.png": fits the overlay's one monospaced line (about 33
/// characters at 11 px) and keeps the end, which is what tells links apart.
fn display_link(url: &str) -> String {
  let trimmed = url.trim_start_matches("https://").trim_start_matches("http://");
  if trimmed.chars().count() <= 32 {
    return trimmed.to_string();
  }
  let head: String = trimmed.chars().take(19).collect();
  let tail: String = trimmed.chars().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
  format!("{head}…{tail}")
}

#[cfg(target_os = "macos")]
fn apply_open_at_login(enabled: bool) -> Result<()> {
  let home = env::var("HOME").context("HOME is not set")?;
  let agent = PathBuf::from(home).join("Library/LaunchAgents/ca.aahmed.screenshotr.plist");
  if !enabled {
    if agent.exists() {
      fs::remove_file(&agent).context("failed to remove login item")?;
    }
    return Ok(());
  }
  let exe = env::current_exe().context("failed to locate the app")?;
  let mut plist = plist::Dictionary::new();
  plist.insert("Label".to_string(), plist::Value::String(KEYRING_SERVICE.to_string()));
  plist.insert(
    "ProgramArguments".to_string(),
    plist::Value::Array(vec![plist::Value::String(exe.display().to_string())]),
  );
  plist.insert("RunAtLoad".to_string(), plist::Value::Boolean(true));
  plist.insert("ProcessType".to_string(), plist::Value::String("Interactive".to_string()));
  if let Some(dir) = agent.parent() {
    fs::create_dir_all(dir).context("failed to create LaunchAgents")?;
  }
  plist::to_file_xml(&agent, &plist).context("failed to write login item")?;
  Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apply_open_at_login(_enabled: bool) -> Result<()> {
  Ok(())
}

#[derive(Serialize)]
struct AppStatus {
  version: String,
  platform: String,
  configured: bool,
  screen_recording: bool,
  hotkeys_registered: bool,
  public_host: String,
  destination_name: String,
  pictures_dir: String,
}

#[tauri::command]
fn get_status(app: AppHandle, state: State<ConfigState>) -> AppStatus {
  let config = state.0.lock().unwrap().clone();
  #[cfg(target_os = "macos")]
  let screen_recording = unsafe { CGPreflightScreenCaptureAccess() };
  #[cfg(not(target_os = "macos"))]
  let screen_recording = true;
  let manager = app.global_shortcut_manager();
  let (full, area) = current_hotkeys(&config);
  let hotkeys_registered = [full, area]
    .iter()
    .filter(|hotkey| !hotkey.is_empty())
    .all(|hotkey| manager.is_registered(hotkey).unwrap_or(false));
  AppStatus {
    version: env!("CARGO_PKG_VERSION").to_string(),
    platform: env::consts::OS.to_string(),
    configured: is_configured(&config),
    screen_recording,
    hotkeys_registered,
    public_host: active_destination(&config).map(|destination| destination.host_label()).unwrap_or_default(),
    destination_name: active_destination(&config).map(|destination| destination.name).unwrap_or_default(),
    pictures_dir: pictures_dir().map(|dir| dir.display().to_string()).unwrap_or_default(),
  }
}

#[tauri::command]
async fn test_destination(destination: Destination) -> Result<String, String> {
  destinations::test(&destination).await.map_err(|error| error.to_string())
}

#[tauri::command]
fn import_sxcu(path: String) -> Result<Destination, String> {
  let contents = fs::read_to_string(&path).map_err(|_| "Couldn't read that file.".to_string())?;
  destinations::import_sxcu(&contents).map_err(|error| error.to_string())
}

#[tauri::command]
async fn connect_r2(token: String, bucket: String) -> Result<Destination, String> {
  destinations::connect_r2(&token, &bucket).await.map_err(|error| error.to_string())
}

/// Adds or replaces a destination and, if asked, makes it the one captures go to.
#[tauri::command]
fn save_destination(
  app: AppHandle,
  state: State<ConfigState>,
  mut destination: Destination,
  activate: bool,
) -> Result<Destination, String> {
  if destination.id.is_empty() {
    destination.id = random_suffix(10);
  }
  if destination.name.trim().is_empty() {
    return Err("Give this destination a name.".to_string());
  }
  let config = {
    let mut config = state.0.lock().unwrap();
    match config.destinations.iter_mut().find(|existing| existing.id == destination.id) {
      Some(existing) => *existing = destination.clone(),
      None => config.destinations.push(destination.clone()),
    }
    if activate || config.active_destination.is_empty() {
      config.active_destination = destination.id.clone();
    }
    config.clone()
  };
  persist_config(&app, &config).map_err(|error| error.to_string())?;
  update_tray(&app);
  Ok(destination)
}

#[tauri::command]
fn delete_destination(app: AppHandle, state: State<ConfigState>, id: String) -> Result<(), String> {
  let config = {
    let mut config = state.0.lock().unwrap();
    config.destinations.retain(|destination| destination.id != id);
    if config.active_destination == id {
      config.active_destination = config.destinations.first().map(|destination| destination.id.clone()).unwrap_or_default();
    }
    config.clone()
  };
  let _ = write_keyring_secret(&destination_secrets_account(&id), "");
  persist_config(&app, &config).map_err(|error| error.to_string())?;
  update_tray(&app);
  Ok(())
}

#[tauri::command]
fn get_config(state: State<ConfigState>) -> AppConfig {
  state.0.lock().unwrap().clone()
}

/// Applies only the fields the window changed, so a partial or stale form can't blank the rest.
#[tauri::command]
fn set_config(
  app: AppHandle,
  state: State<ConfigState>,
  changes: serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
  let previous = state.0.lock().unwrap().clone();
  let mut merged = match serde_json::to_value(&previous) {
    Ok(serde_json::Value::Object(fields)) => fields,
    _ => return Err("Couldn't read the current settings.".to_string()),
  };
  for (key, value) in changes {
    if !merged.contains_key(&key) {
      return Err(format!("Unknown setting: {key}"));
    }
    merged.insert(key, value);
  }
  let config: AppConfig =
    serde_json::from_value(serde_json::Value::Object(merged)).map_err(|error| format!("Invalid setting: {error}"))?;
  let hotkeys_changed = previous.hotkey_full != config.hotkey_full
    || previous.hotkey_area != config.hotkey_area
    || previous.hotkey_clipboard != config.hotkey_clipboard;

  if hotkeys_changed {
    let set: Vec<&str> = [&config.hotkey_full, &config.hotkey_area, &config.hotkey_clipboard]
      .iter()
      .map(|hotkey| hotkey.trim())
      .filter(|hotkey| !hotkey.is_empty())
      .collect();
    if (1..set.len()).any(|i| set[..i].contains(&set[i])) {
      return Err("Each shortcut needs to be different.".to_string());
    }
    *state.0.lock().unwrap() = config.clone();
    #[cfg(target_os = "macos")]
    if let Err(error) = macos_sync_system_shortcuts(&app, &config) {
      println!("[hotkeys] failed to sync system shortcuts: {error}");
    }
    let mut manager = app.global_shortcut_manager();
    let _ = manager.unregister_all();
    if let Err(error) = register_shortcuts(&app) {
      // Put the working shortcuts back instead of saving one that doesn't register.
      println!("[hotkeys] {error:#}; restoring previous shortcuts");
      *state.0.lock().unwrap() = previous.clone();
      let _ = manager.unregister_all();
      #[cfg(target_os = "macos")]
      let _ = macos_sync_system_shortcuts(&app, &previous);
      let _ = register_shortcuts(&app);
      update_tray(&app);
      return Err("That shortcut is in use by macOS or another app. Try a different one.".to_string());
    }
  } else {
    *state.0.lock().unwrap() = config.clone();
  }

  if previous.open_at_login != config.open_at_login {
    apply_open_at_login(config.open_at_login).map_err(|error| error.to_string())?;
  }
  persist_config(&app, &config).map_err(|e| e.to_string())?;
  update_tray(&app);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn formats_links_for_the_clipboard() {
    let url = "https://i.example.com/a.png";
    assert_eq!(formatted_link(url, "url"), (url.to_string(), "Link copied"));
    assert_eq!(formatted_link(url, "markdown").0, "![Screenshot](https://i.example.com/a.png)");
    assert_eq!(formatted_link(url, "html").0, "<img src=\"https://i.example.com/a.png\" alt=\"Screenshot\">");
    assert_eq!(formatted_link(url, "").1, "Link copied");
  }

  #[test]
  fn names_stored_files_readably() {
    let name = stored_file_name("Quarterly Report (final).PDF");
    assert!(name.starts_with("quarterly-report-final-"), "{name}");
    assert!(name.ends_with(".pdf"), "{name}");
    assert!(stored_file_name("???").starts_with("file-"));
    let no_ext = stored_file_name("Makefile");
    assert!(no_ext.starts_with("makefile-") && !no_ext.contains('.'));
  }

  #[test]
  fn shortens_links_to_one_overlay_line() {
    let short = "https://i.example.com/a.png";
    assert_eq!(display_link(short), "i.example.com/a.png");
    let long = display_link("https://i.aahmed.ca/screenshot-1792634551000-k2Jd8fQa.png");
    assert_eq!(long, "i.aahmed.ca/screens…k2Jd8fQa.png");
    assert!(long.chars().count() <= 32);
  }
}
