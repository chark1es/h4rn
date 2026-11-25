//! h4rn mobile shell.
//!
//! Same contract as the desktop shell (ADR-001): a thin Tauri layer over the
//! shared React bundle. Product behaviour stays behind the daemon's WebSocket
//! protocol; the native side only owns what a WebView cannot do:
//!
//! - `secret_get` / `secret_set` / `secret_delete`: Android Keystore- and iOS
//!   Keychain-backed storage for the daemon pairing token (ADR-016);
//! - `deep_link_current`: the `h4rn://` URL that cold-started the app, so the
//!   connect screen can pair from `h4rn://connect?daemon=…&token=…` and the
//!   OIDC callback can be forwarded to the UI;
//! - QR scanning through the barcode-scanner plugin (called by the UI);
//! - the Android back button, mapped onto WebView history in MainActivity.
//!
//! The daemon itself does not run on the phone; the app connects to a daemon on
//! another host over ws/wss. `scripts/mobile-android.sh` wires up the emulator.

#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri::Manager;
#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri::plugin::PluginHandle;

mod preview;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_h4rn_secret_store);

/// Upper bound for a logical secret key.
const MAX_SECRET_KEY: usize = 256;
/// Upper bound for one secret, matching the desktop keychain command.
const MAX_SECRET_VALUE: usize = 64 * 1024;

#[cfg(any(target_os = "android", target_os = "ios"))]
struct SecretStore(PluginHandle<tauri::Wry>);

/// Android FCM bridge (ADR-049). The plugin is Android-only; iOS alerts are a
/// later task, so the commands report the feature unavailable there.
#[cfg(target_os = "android")]
struct PushBridge(PluginHandle<tauri::Wry>);

/// The command's answer on every target; the host build reports unavailable.
#[derive(serde::Serialize, serde::Deserialize)]
struct PushStatusValue {
    available: bool,
    permission: String,
    token: Option<String>,
    #[allow(dead_code)]
    channel: bool,
    /// Filled by the shell from its own crate version.
    #[serde(default)]
    app_version: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(serde::Serialize)]
struct PushForegroundArgs<'a> {
    #[serde(rename = "threadId")]
    thread_id: Option<&'a str>,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(serde::Deserialize)]
struct PushTokenValue {
    token: String,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(serde::Serialize)]
struct SecretArgs<'a> {
    key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a str>,
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[derive(serde::Deserialize)]
struct SecretValue {
    value: Option<String>,
}

fn validate_secret_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("the secret key must not be empty".into());
    }
    if key.len() > MAX_SECRET_KEY {
        return Err(format!(
            "the secret key is longer than {MAX_SECRET_KEY} bytes"
        ));
    }
    Ok(())
}

/// Reads a secret from the platform keystore.
#[tauri::command]
fn secret_get(app: tauri::AppHandle, key: String) -> Result<Option<String>, String> {
    validate_secret_key(&key)?;

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let store = app.state::<SecretStore>();
        store
            .0
            .run_mobile_plugin::<SecretValue>(
                "secretGet",
                SecretArgs {
                    key: &key,
                    value: None,
                },
            )
            .map(|secret| secret.value)
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = app;
        Err("secret storage is only implemented on the mobile shells".into())
    }
}

/// Writes or replaces a secret in the platform keystore.
#[tauri::command]
fn secret_set(app: tauri::AppHandle, key: String, value: String) -> Result<(), String> {
    validate_secret_key(&key)?;
    if value.len() > MAX_SECRET_VALUE {
        return Err(format!(
            "the secret is larger than {MAX_SECRET_VALUE} bytes"
        ));
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let store = app.state::<SecretStore>();
        store
            .0
            .run_mobile_plugin::<()>(
                "secretSet",
                SecretArgs {
                    key: &key,
                    value: Some(&value),
                },
            )
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = (app, value);
        Err("secret storage is only implemented on the mobile shells".into())
    }
}

/// Deletes a secret; missing keys are not an error.
#[tauri::command]
fn secret_delete(app: tauri::AppHandle, key: String) -> Result<(), String> {
    validate_secret_key(&key)?;

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let store = app.state::<SecretStore>();
        store
            .0
            .run_mobile_plugin::<()>(
                "secretDelete",
                SecretArgs {
                    key: &key,
                    value: None,
                },
            )
            .map_err(|error| error.to_string())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = app;
        Err("secret storage is only implemented on the mobile shells".into())
    }
}

/// Reports whether this build can push, the OS permission, and a cached token.
#[tauri::command]
fn push_status(app: tauri::AppHandle) -> Result<PushStatusValue, String> {
    #[cfg(target_os = "android")]
    {
        let bridge = app.state::<PushBridge>();
        let mut status = bridge
            .0
            .run_mobile_plugin::<PushStatusValue>("pushStatus", ())
            .map_err(|error| error.to_string())?;
        status.app_version = env!("CARGO_PKG_VERSION").to_string();
        Ok(status)
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(PushStatusValue {
            available: false,
            permission: "default".into(),
            token: None,
            channel: false,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }
}

/// Asks for the Android 13+ notification permission.
#[tauri::command]
fn push_request_permission(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let bridge = app.state::<PushBridge>();
        bridge
            .0
            .run_mobile_plugin::<()>("pushRequestPermission", ())
            .map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("device alerts are only implemented on Android".into())
    }
}

/// Returns the current FCM registration token.
#[tauri::command]
fn push_token(app: tauri::AppHandle) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        let bridge = app.state::<PushBridge>();
        bridge
            .0
            .run_mobile_plugin::<PushTokenValue>("pushToken", ())
            .map(|value| value.token)
            .map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("device alerts are only implemented on Android".into())
    }
}

/// Tells the notification service which thread is on screen, so only that
/// exact thread stays quiet in the foreground.
#[tauri::command]
fn push_set_foreground_thread(
    app: tauri::AppHandle,
    thread_id: Option<String>,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let bridge = app.state::<PushBridge>();
        bridge
            .0
            .run_mobile_plugin::<()>(
                "pushSetForegroundThread",
                PushForegroundArgs {
                    thread_id: thread_id.as_deref(),
                },
            )
            .map_err(|error| error.to_string())
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, thread_id);
        Ok(())
    }
}

/// Returns the `h4rn://` URLs that launched this process, if any.
///
/// Android delivers a cold-start deep link through the launching intent rather
/// than the plugin's new-URL event, so the connect screen asks for it once at
/// startup.
#[tauri::command]
fn deep_link_current(app: tauri::AppHandle) -> Vec<String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        use tauri_plugin_deep_link::DeepLinkExt;
        app.deep_link()
            .get_current()
            .ok()
            .flatten()
            .map(|urls| urls.into_iter().map(|url| url.to_string()).collect())
            .unwrap_or_default()
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = app;
        Vec::new()
    }
}

/// Build and run the mobile shell.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        // A prepared preview gets its own origin, because a `srcdoc` frame
        // would inherit this window's `script-src 'self'` policy.
        .manage(preview::PreviewStore::default())
        .register_uri_scheme_protocol(preview::PREVIEW_SCHEME, |context, request| {
            use tauri::Manager as _;
            let store = context.app_handle().state::<preview::PreviewStore>();
            preview::respond(&store, &request)
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("h4rn-secret-store")
                .setup(|app, api| {
                    #[cfg(target_os = "android")]
                    {
                        let handle =
                            api.register_android_plugin("dev.h4rn.app", "SecretStorePlugin")?;
                        app.manage(SecretStore(handle));
                    }
                    #[cfg(target_os = "ios")]
                    {
                        let handle = api.register_ios_plugin(init_plugin_h4rn_secret_store)?;
                        app.manage(SecretStore(handle));
                    }
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    {
                        let _ = (app, api);
                    }
                    Ok(())
                })
                .build(),
        )
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("h4rn-push")
                .setup(|app, api| {
                    #[cfg(target_os = "android")]
                    {
                        let handle = api.register_android_plugin("dev.h4rn.app", "PushPlugin")?;
                        app.manage(PushBridge(handle));
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        let _ = (app, api);
                    }
                    Ok(())
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            secret_get,
            secret_set,
            secret_delete,
            push_status,
            push_request_permission,
            push_token,
            push_set_foreground_thread,
            deep_link_current,
            preview::preview_capabilities,
            preview::preview_prepare,
            preview::preview_release
        ])
        .setup(|_app| {
            // Warm-start deep links are pushed into the WebView as a DOM event
            // (and a Tauri event for future listeners) instead of relying on
            // the UI importing the deep-link JS package.
            #[cfg(any(target_os = "android", target_os = "ios"))]
            {
                use tauri::Emitter;
                use tauri_plugin_deep_link::DeepLinkExt;

                let handle = _app.handle().clone();
                _app.deep_link().on_open_url(move |event| {
                    let urls: Vec<String> =
                        event.urls().into_iter().map(|url| url.to_string()).collect();
                    if let Some(window) = handle.get_webview_window("main") {
                        let _ = window.emit("h4rn://deep-link", urls.clone());
                        if let Ok(detail) = serde_json::to_string(&urls) {
                            let _ = window.eval(format!(
                                "window.dispatchEvent(new CustomEvent('h4rn-deep-link', {{ detail: {detail} }}));"
                            ));
                        }
                    }
                });
            }
            Ok(())
        });

    #[cfg(any(target_os = "android", target_os = "ios"))]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    builder
        .run(tauri::generate_context!())
        .expect("error while running the h4rn mobile shell");
}
