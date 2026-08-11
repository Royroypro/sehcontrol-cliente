use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(not(any(target_os = "ios")))]
use crate::{ui_interface::get_builtin_option, Connection};
use hbb_common::{
    config::{self, keys, Config, LocalConfig},
    log,
    tokio::{self, sync::broadcast, time::Instant},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const TIME_HEARTBEAT: Duration = Duration::from_secs(15);
const UPLOAD_SYSINFO_TIMEOUT: Duration = Duration::from_secs(120);
const TIME_CONN: Duration = Duration::from_secs(3);

#[cfg(not(any(target_os = "ios")))]
lazy_static::lazy_static! {
    static ref SENDER : Mutex<broadcast::Sender<Vec<i32>>> = Mutex::new(start_hbbs_sync());
    static ref PRO: Arc<Mutex<bool>> = Default::default();
}

#[cfg(not(any(target_os = "ios")))]
pub fn start() {
    let _sender = SENDER.lock().unwrap();
}

#[cfg(not(target_os = "ios"))]
pub fn signal_receiver() -> broadcast::Receiver<Vec<i32>> {
    SENDER.lock().unwrap().subscribe()
}

#[cfg(not(any(target_os = "ios")))]
fn start_hbbs_sync() -> broadcast::Sender<Vec<i32>> {
    let (tx, _rx) = broadcast::channel::<Vec<i32>>(16);
    std::thread::spawn(move || start_hbbs_sync_async());
    return tx;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StrategyOptions {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config_options: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, String>,
}

struct InfoUploaded {
    uploaded: bool,
    url: String,
    last_uploaded: Option<Instant>,
    id: String,
    username: Option<String>,
}

impl Default for InfoUploaded {
    fn default() -> Self {
        Self {
            uploaded: false,
            url: "".to_owned(),
            last_uploaded: None,
            id: "".to_owned(),
            username: None,
        }
    }
}

impl InfoUploaded {
    fn uploaded(url: String, id: String, username: String) -> Self {
        Self {
            uploaded: true,
            url,
            last_uploaded: None,
            id,
            username: Some(username),
        }
    }
}

/// Mirrors `UserModel._readScreenCamStatus` in `flutter/lib/models/user_model.dart`
/// exactly (same keys, same "omit empty" rules, same `actual_state` collapsed
/// to `running`/`stopped` for the server's documented contract — see
/// docs/SCREENCAM_PLAN.md section 12, point 2) — but for Windows/desktop,
/// which the Dart version never reaches: `UserModel._startHeartbeat` early-
/// returns unless `isAndroid`, since Android needs a Dart-driven heartbeat as
/// a fallback for when its background service isn't reliably alive, while
/// desktop's `--server` process (where this function also runs) already has
/// this native heartbeat loop. Without this, screen_cam status silently
/// never reached the panel for any Windows client (found 27/07, alongside
/// the LocalConfig cross-process staleness bug — see screen_cam/mod.rs).
///
/// Reads `LocalConfig::get_option` directly, not `get_option_from_file`:
/// this runs in the exact same `--server` process as `screen_cam::mod.rs`
/// (both started from `start_server`'s `is_server` branch in src/server.rs),
/// so there's no cross-process staleness to work around here — the cached
/// copy is already correct.
#[cfg(all(windows, feature = "screencam"))]
fn screen_cam_status() -> Option<Value> {
    let display_status = crate::server::screen_cam::heartbeat_display_status();
    Some(build_screen_cam_status(ScreenCamHeartbeatStatus {
        raw_state: LocalConfig::get_option("screencam-actual-state"),
        licensed: LocalConfig::get_option_from_file("screencam-licensed") == "Y",
        desired_state: LocalConfig::get_option_from_file("screencam-desired-state"),
        encoder: LocalConfig::get_option("screencam-encoder"),
        last_error: LocalConfig::get_option("screencam-last-error"),
        rtsp_clients: LocalConfig::get_option("screencam-rtsp-clients")
            .parse::<i64>()
            .ok(),
        local_ip: LocalConfig::get_option("screencam-local-ip"),
        rtsp_port: LocalConfig::get_option("screencam-rtsp-port")
            .parse::<i64>()
            .ok(),
        rtsp_user: LocalConfig::get_option_from_file("screencam-rtsp-user"),
        display_status: display_status
            .unwrap_or_else(crate::server::screen_cam::heartbeat_initial_display_status),
    }))
}

#[cfg(all(windows, feature = "screencam"))]
struct ScreenCamHeartbeatStatus {
    raw_state: String,
    licensed: bool,
    desired_state: String,
    encoder: String,
    last_error: String,
    rtsp_clients: Option<i64>,
    local_ip: String,
    rtsp_port: Option<i64>,
    rtsp_user: String,
    display_status: Value,
}

#[cfg(all(windows, feature = "screencam"))]
fn build_screen_cam_status(status: ScreenCamHeartbeatStatus) -> Value {
    let actual_state = if status.raw_state == "running" {
        "running"
    } else {
        "stopped"
    };
    let mut v = serde_json::Map::new();
    v.insert("actual_state".to_owned(), json!(actual_state));
    v.insert(
        "status".to_owned(),
        json!(if status.raw_state.is_empty() {
            "stopped"
        } else {
            status.raw_state.as_str()
        }),
    );
    v.insert("licensed".to_owned(), json!(status.licensed));
    v.insert(
        "desired_state".to_owned(),
        json!(if status.desired_state == "running" {
            "running"
        } else {
            "stopped"
        }),
    );
    if !status.encoder.is_empty() {
        v.insert("encoder".to_owned(), json!(status.encoder));
    }
    v.insert(
        "last_error".to_owned(),
        if status.last_error.is_empty() {
            Value::Null
        } else {
            json!(status.last_error)
        },
    );
    if let Some(rtsp_clients) = status.rtsp_clients {
        v.insert("rtsp_clients".to_owned(), json!(rtsp_clients));
    }
    if !status.local_ip.is_empty() {
        v.insert("local_ip".to_owned(), json!(status.local_ip));
    }
    if let Some(rtsp_port) = status.rtsp_port {
        v.insert("rtsp_port".to_owned(), json!(rtsp_port));
    }
    // Lets the panel confirm the credentials it issued actually reached this
    // device, without ever echoing them back: a device reporting
    // `auth_enabled: false` while the panel thinks it sent a user is a
    // misconfiguration the admin needs to see. Only the username is reported,
    // never the password.
    //
    // Unlike every key above, this one is written by the *UI* process (Dart's
    // `_persistScreenCamPolicyHistory`), not by screen_cam in this process — so the
    // cached `get_option` would never observe a credential rotation and the
    // panel would be told "no auth" forever. Same cross-process staleness the
    // module doc above describes, hence the from-file read here.
    v.insert(
        "auth_enabled".to_owned(),
        json!(!status.rtsp_user.is_empty()),
    );
    if !status.rtsp_user.is_empty() {
        v.insert("rtsp_user".to_owned(), json!(status.rtsp_user));
    }
    if let Value::Object(display_status) = status.display_status {
        // `heartbeat_display_status` clones one DisplayRuntimeState while its
        // lock is held and serializes only after releasing it, so these five
        // fields always describe the same topology/selection instant.
        v.extend(display_status);
    }
    Value::Object(v)
}

#[cfg(not(all(windows, feature = "screencam")))]
fn screen_cam_status() -> Option<Value> {
    None
}

#[cfg(all(test, windows, feature = "screencam"))]
mod screen_cam_heartbeat_tests {
    use super::*;

    fn status_with_display(display_status: Value) -> ScreenCamHeartbeatStatus {
        ScreenCamHeartbeatStatus {
            raw_state: String::new(),
            licensed: false,
            desired_state: String::new(),
            encoder: String::new(),
            last_error: String::new(),
            rtsp_clients: None,
            local_ip: String::new(),
            rtsp_port: None,
            rtsp_user: String::new(),
            display_status,
        }
    }

    #[test]
    fn heartbeat_envelope_always_contains_screen_cam_shape_without_live_state() {
        let screen_cam = build_screen_cam_status(status_with_display(json!({
            "available_displays": [],
            "selected_display_id": null,
            "active_display_id": null,
            "fallback_active": false,
            "display_warning": null,
        })));
        let envelope = json!({"screen_cam": screen_cam});
        let status = &envelope["screen_cam"];
        assert_eq!(status["status"], "stopped");
        assert_eq!(status["actual_state"], "stopped");
        assert_eq!(status["licensed"], false);
        assert_eq!(status["desired_state"], "stopped");
        assert!(status["available_displays"].as_array().unwrap().is_empty());
        assert!(status["selected_display_id"].is_null());
        assert!(status["active_display_id"].is_null());
        assert_eq!(status["fallback_active"], false);
        assert!(status["display_warning"].is_null());
    }

    #[test]
    fn productive_heartbeat_constructor_never_omits_screen_cam() {
        let screen_cam = screen_cam_status().expect("Windows ScreenCam heartbeat must exist");
        for field in [
            "available_displays",
            "selected_display_id",
            "active_display_id",
            "fallback_active",
            "display_warning",
        ] {
            assert!(screen_cam.get(field).is_some(), "missing field {field}");
        }
    }

    #[test]
    fn heartbeat_envelope_preserves_persisted_selection_fallback_and_warning() {
        let mut input = status_with_display(json!({
            "available_displays": [],
            "selected_display_id": r"\\.\DISPLAY7",
            "active_display_id": null,
            "fallback_active": true,
            "display_warning": "selected display is unavailable",
        }));
        input.raw_state = "disabled".to_owned();
        input.licensed = true;
        input.desired_state = "running".to_owned();
        let screen_cam = build_screen_cam_status(input);
        assert_eq!(screen_cam["status"], "disabled");
        assert_eq!(screen_cam["actual_state"], "stopped");
        assert_eq!(screen_cam["licensed"], true);
        assert_eq!(screen_cam["desired_state"], "running");
        assert_eq!(screen_cam["selected_display_id"], r"\\.\DISPLAY7");
        assert!(screen_cam["active_display_id"].is_null());
        assert_eq!(screen_cam["fallback_active"], true);
        assert_eq!(
            screen_cam["display_warning"],
            "selected display is unavailable"
        );
    }
}

#[cfg(not(any(target_os = "ios")))]
#[tokio::main(flavor = "current_thread")]
async fn start_hbbs_sync_async() {
    let mut interval = crate::rustdesk_interval(tokio::time::interval_at(
        Instant::now() + TIME_CONN,
        TIME_CONN,
    ));
    let mut last_sent: Option<Instant> = None;
    let mut info_uploaded = InfoUploaded::default();
    let mut sysinfo_ver = "".to_owned();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let url = heartbeat_url();
                let id = Config::get_id();
                if url.is_empty() {
                    *PRO.lock().unwrap() = false;
                    continue;
                }
                if config::option2bool("stop-service", &Config::get_option("stop-service")) {
                    continue;
                }
                let conns = Connection::alive_conns();
                if info_uploaded.uploaded && (url != info_uploaded.url || id != info_uploaded.id) {
                    info_uploaded.uploaded = false;
                    *PRO.lock().unwrap() = false;
                }
                // For Windows:
                // We can't skip uploading sysinfo when the username is empty, because the username may
                // always be empty before login. We also need to upload the other sysinfo info.
                //
                // https://github.com/rustdesk/rustdesk/discussions/8031
                // We still need to check the username after uploading sysinfo, because
                // 1. The username may be empty when logining in, and it can be fetched after a while.
                //    In this case, we need to upload sysinfo again.
                // 2. The username may be changed after uploading sysinfo, and we need to upload sysinfo again.
                //
                // The Windows session will switch to the last user session before the restart,
                // so it may be able to get the username before login.
                // But strangely, sometimes we can get the username before login,
                // we may not be able to get the username before login after the next restart.
                let mut v = crate::get_sysinfo();
                let sys_username = v["username"].as_str().unwrap_or_default().to_string();
                // Though the username comparison is only necessary on Windows,
                // we still keep the comparison on other platforms for consistency.
                let need_upload = (!info_uploaded.uploaded || info_uploaded.username.as_ref() != Some(&sys_username)) &&
                    info_uploaded.last_uploaded.map(|x| x.elapsed() >= UPLOAD_SYSINFO_TIMEOUT).unwrap_or(true);
                if need_upload {
                    v["version"] = json!(crate::VERSION);
                    v["id"] = json!(id);
                    v["uuid"] = json!(crate::encode64(hbb_common::get_uuid()));
                    let ab_name = Config::get_option(keys::OPTION_PRESET_ADDRESS_BOOK_NAME);
                    if !ab_name.is_empty() {
                        v[keys::OPTION_PRESET_ADDRESS_BOOK_NAME] = json!(ab_name);
                    }
                    let ab_tag = Config::get_option(keys::OPTION_PRESET_ADDRESS_BOOK_TAG);
                    if !ab_tag.is_empty() {
                        v[keys::OPTION_PRESET_ADDRESS_BOOK_TAG] = json!(ab_tag);
                    }
                    let ab_alias = Config::get_option(keys::OPTION_PRESET_ADDRESS_BOOK_ALIAS);
                    if !ab_alias.is_empty() {
                        v[keys::OPTION_PRESET_ADDRESS_BOOK_ALIAS] = json!(ab_alias);
                    }
                    let ab_password = Config::get_option(keys::OPTION_PRESET_ADDRESS_BOOK_PASSWORD);
                    if !ab_password.is_empty() {
                        v[keys::OPTION_PRESET_ADDRESS_BOOK_PASSWORD] = json!(ab_password);
                    }
                    let ab_note = Config::get_option(keys::OPTION_PRESET_ADDRESS_BOOK_NOTE);
                    if !ab_note.is_empty() {
                        v[keys::OPTION_PRESET_ADDRESS_BOOK_NOTE] = json!(ab_note);
                    }
                    let username = get_builtin_option(keys::OPTION_PRESET_USERNAME);
                    if !username.is_empty() {
                        v[keys::OPTION_PRESET_USERNAME] = json!(username);
                    }
                    let strategy_name = get_builtin_option(keys::OPTION_PRESET_STRATEGY_NAME);
                    if !strategy_name.is_empty() {
                        v[keys::OPTION_PRESET_STRATEGY_NAME] = json!(strategy_name);
                    }
                    let device_group_name = get_builtin_option(keys::OPTION_PRESET_DEVICE_GROUP_NAME);
                    if !device_group_name.is_empty() {
                        v[keys::OPTION_PRESET_DEVICE_GROUP_NAME] = json!(device_group_name);
                    }
                    let device_username = Config::get_option(keys::OPTION_PRESET_DEVICE_USERNAME);
                    if !device_username.is_empty() {
                        v["username"] = json!(device_username);
                    }
                    let device_name = Config::get_option(keys::OPTION_PRESET_DEVICE_NAME);
                    if !device_name.is_empty() {
                        v["hostname"] = json!(device_name);
                    }
                    let note = Config::get_option(keys::OPTION_PRESET_NOTE);
                    if !note.is_empty() {
                        v[keys::OPTION_PRESET_NOTE] = json!(note);
                    }
                    let v = v.to_string();
                    let mut hash = "".to_owned();
                    if crate::is_public(&url) {
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(url.as_bytes());
                        hasher.update(&v.as_bytes());
                        let res = hasher.finalize();
                        hash = hbb_common::base64::encode(&res[..]);
                        let old_hash = config::Status::get("sysinfo_hash");
                        let ver = config::Status::get("sysinfo_ver"); // sysinfo_ver is the version of sysinfo on server's side
                        if hash == old_hash {
                            // When the api doesn't exist, Ok("") will be returned in test.
                            let samever = match crate::post_request(url.replace("heartbeat", "sysinfo_ver"), "".to_owned(), "").await {
                                Ok(x)  => {
                                    sysinfo_ver = x.clone();
                                    *PRO.lock().unwrap() = true;
                                    x == ver
                                }
                                _ => {
                                    false // to make sure Pro can be assigned in below post for old
                                            // hbbs pro not supporting sysinfo_ver, use false for ensuring
                                }
                            };
                            if samever {
                                info_uploaded = InfoUploaded::uploaded(url.clone(), id.clone(), sys_username);
                                log::info!("sysinfo not changed, skip upload");
                                continue;
                            }
                        }
                    }
                    match crate::post_request(url.replace("heartbeat", "sysinfo"), v, "").await {
                        Ok(x)  => {
                            if x == "SYSINFO_UPDATED" {
                                info_uploaded = InfoUploaded::uploaded(url.clone(), id.clone(), sys_username);
                                log::info!("sysinfo updated");
                                if !hash.is_empty() {
                                    config::Status::set("sysinfo_hash", hash);
                                    config::Status::set("sysinfo_ver", sysinfo_ver.clone());
                                }
                                *PRO.lock().unwrap() = true;
                            } else if x == "ID_NOT_FOUND" {
                                info_uploaded.last_uploaded = None; // next heartbeat will upload sysinfo again
                            } else {
                                info_uploaded.last_uploaded = Some(Instant::now());
                            }
                        }
                        _ => {
                            info_uploaded.last_uploaded = Some(Instant::now());
                        }
                    }
                }
                if conns.is_empty() && last_sent.map(|x| x.elapsed() < TIME_HEARTBEAT).unwrap_or(false) {
                    continue;
                }
                last_sent = Some(Instant::now());
                let mut v = Value::default();
                v["id"] = json!(id);
                v["uuid"] = json!(crate::encode64(hbb_common::get_uuid()));
                v["ver"] = json!(hbb_common::get_version_number(crate::VERSION));
                if !conns.is_empty() {
                    v["conns"] = json!(conns);
                }
                let modified_at = LocalConfig::get_option("strategy_timestamp").parse::<i64>().unwrap_or(0);
                v["modified_at"] = json!(modified_at);
                if let Some(screen_cam) = screen_cam_status() {
                    v["screen_cam"] = screen_cam;
                }
                if let Ok(s) = crate::post_request(url.clone(), v.to_string(), "").await {
                    if let Ok(mut rsp) = serde_json::from_str::<HashMap::<&str, Value>>(&s) {
                        if rsp.remove("sysinfo").is_some() {
                            info_uploaded.uploaded = false;
                            config::Status::set("sysinfo_hash", "".to_owned());
                            log::info!("sysinfo required to forcely update");
                        }
                        if let Some(conns)  = rsp.remove("disconnect") {
                                if let Ok(conns) = serde_json::from_value::<Vec<i32>>(conns) {
                                    SENDER.lock().unwrap().send(conns).ok();
                                }
                        }
                        if let Some(rsp_modified_at) = rsp.remove("modified_at") {
                            if let Ok(rsp_modified_at) = serde_json::from_value::<i64>(rsp_modified_at) {
                                if rsp_modified_at != modified_at {
                                    LocalConfig::set_option("strategy_timestamp".to_string(), rsp_modified_at.to_string());
                                }
                            }
                        }
                        if let Some(strategy) = rsp.remove("strategy") {
                            if let Ok(strategy) = serde_json::from_value::<StrategyOptions>(strategy) {
                                log::info!("strategy updated");
                                handle_config_options(strategy.config_options);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn heartbeat_url() -> String {
    let url = crate::common::get_api_server(
        Config::get_option("api-server"),
        Config::get_option("custom-rendezvous-server"),
    );
    if url.is_empty() || crate::is_public(&url) {
        return "".to_owned();
    }
    format!("{}/api/heartbeat", url)
}

fn handle_config_options(config_options: HashMap<String, String>) {
    let mut options = Config::get_options();
    let default_settings = config::DEFAULT_SETTINGS.read().unwrap().clone();
    config_options
        .iter()
        .map(|(k, v)| {
            // Priority: user config > default advanced options.
            // Only when default advanced options are also empty, remove user option (fallback to built-in default);
            // otherwise insert an empty value so user config remains present.
            if v.is_empty() && default_settings.get(k).map_or("", |v| v).is_empty() {
                options.remove(k);
            } else {
                options.insert(k.to_string(), v.to_string());
            }
        })
        .count();
    Config::set_options(options);
}

#[allow(unused)]
#[cfg(not(any(target_os = "ios")))]
pub fn is_pro() -> bool {
    PRO.lock().unwrap().clone()
}

// Fire-and-forget by design: the switch flow must not block on this POST.
// If the device clock is outside the server's accepted window, the server
// returns its current Unix time and this task re-signs and retries once.
#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn register_switch_grant(switch_uuid: String) {
    tokio::spawn(async move {
        let api_server = crate::ui_interface::get_api_server();
        if api_server.is_empty() || crate::is_public(&api_server) {
            return;
        }
        use hbb_common::sodiumoxide::crypto::{hash::sha256, sign};
        let switch_code = crate::encode64(sha256::hash(switch_uuid.as_bytes()).0);
        let switch_code_verifier = switch_code_verifier(&switch_code);
        let timestamp = (hbb_common::get_time() / 1000).to_string();
        let id = Config::get_id();
        let kp = Config::get_key_pair();
        let Some(sk) = sign::SecretKey::from_slice(&kp.0) else {
            log::error!("Failed to register switch grant: no device key");
            return;
        };
        let url = format!("{}/api/switch-grant", api_server);
        let mut timestamp = timestamp;
        for attempt in 0..2 {
            let signature = sign::sign_detached(
                &switch_grant_signed_msg(&id, &switch_code_verifier, &timestamp),
                &sk,
            );
            let body = json!({
                "id": &id,
                "switch_code_verifier": &switch_code_verifier,
                "timestamp": &timestamp,
                "signature": crate::encode64(signature.to_bytes()),
            })
            .to_string();
            let response = match crate::post_request(url.clone(), body, "").await {
                Ok(response) => response,
                Err(e) => {
                    log::error!("Failed to register switch grant: {}", e);
                    return;
                }
            };
            let response = match serde_json::from_str::<Value>(&response) {
                Ok(response) => response,
                Err(e) => {
                    log::error!("Failed to register switch grant: invalid response: {}", e);
                    return;
                }
            };
            match response.get("accepted").and_then(Value::as_bool) {
                Some(true) => return,
                Some(false) => {}
                None => {
                    log::error!("Failed to register switch grant: missing accepted response");
                    return;
                }
            }
            let Some(server_time) = response["server_time"].as_i64() else {
                log::error!("Failed to register switch grant: rejected by server");
                return;
            };
            if attempt == 0 {
                log::warn!("Switch grant timestamp rejected, retrying with server time");
                timestamp = server_time.to_string();
            } else {
                log::error!("Failed to register switch grant after retrying with server time");
            }
        }
    });
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn switch_code_verifier(switch_code: &str) -> String {
    use hbb_common::sodiumoxide::crypto::hash::sha256;

    let prefix = b"switch-grant-verifier\0";
    let mut msg = Vec::with_capacity(prefix.len() + switch_code.len());
    msg.extend_from_slice(prefix);
    msg.extend_from_slice(switch_code.as_bytes());
    crate::encode64(sha256::hash(&msg).0)
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn switch_grant_signed_msg(id: &str, switch_code_verifier: &str, timestamp: &str) -> Vec<u8> {
    let mut msg =
        Vec::with_capacity(13 + id.len() + 1 + switch_code_verifier.len() + 1 + timestamp.len());
    msg.extend_from_slice(b"switch-grant\0");
    msg.extend_from_slice(id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(switch_code_verifier.as_bytes());
    msg.push(0);
    msg.extend_from_slice(timestamp.as_bytes());
    msg
}

#[cfg(all(
    test,
    feature = "flutter",
    not(any(target_os = "android", target_os = "ios"))
))]
mod tests {
    use super::{switch_code_verifier, switch_grant_signed_msg};

    #[test]
    fn test_switch_code_verifier_is_not_raw_switch_code() {
        let switch_code = "code-abc";
        let verifier = switch_code_verifier(switch_code);
        assert_ne!(verifier, switch_code);
        assert_eq!(verifier, switch_code_verifier(switch_code));
        assert_eq!(
            verifier,
            "dMIn3uiPe77XodFB5IKi7PrKJ7l7+zVquNn0ObSaHQc="
        );
    }

    #[test]
    fn test_switch_grant_signed_msg_layout() {
        let expected: Vec<u8> = [
            &b"switch-grant\0"[..],
            b"id1",
            b"\0",
            b"c1",
            b"\0",
            b"1700000000",
        ]
        .concat();
        assert_eq!(switch_grant_signed_msg("id1", "c1", "1700000000"), expected);
    }
}
