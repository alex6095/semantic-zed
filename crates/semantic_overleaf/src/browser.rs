//! Native browser sign-in for the Overleaf integration.
//!
//! The editor never reads, copies, or opens a user's personal browser profile.
//! Instead it starts a Chromium-compatible browser with a private, app-owned
//! profile and an unauthenticated loopback-only DevTools endpoint. That profile
//! persists its own SSO state, so a later Overleaf reauthentication normally
//! needs no identity-provider password, while the app can only read the
//! cookies from the window it started.

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_tungstenite::tokio::connect_async;
use async_tungstenite::tungstenite::Message;
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::process::{Child, Command as TokioCommand};
use tokio::time::sleep;
use url::Url;

use crate::credentials::{CredentialError, CredentialRecord, CredentialStore};
use crate::http::{HttpError, OverleafHttpClient, canonical_server_url};

const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEVTOOLS_START_TIMEOUT: Duration = Duration::from_secs(30);
const DEVTOOLS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEVTOOLS_HTTP_TIMEOUT: Duration = Duration::from_secs(1);
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(750);
const BROWSER_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// The error surface intentionally never includes cookies or browser profile
/// paths. The caller can present these messages directly in the native panel.
#[derive(Debug, Error)]
pub enum BrowserLoginError {
    #[error("The Overleaf server URL is invalid: {0}")]
    InvalidServer(#[from] url::ParseError),
    #[error("Could not prepare Semantic Zed's private browser profile: {0}")]
    Profile(#[source] std::io::Error),
    #[error("No compatible Chromium browser is available for the secure Overleaf sign-in. {hint}")]
    NoCompatibleBrowser { hint: &'static str },
    #[error("Could not reserve a local browser-login port: {0}")]
    ReservePort(#[source] std::io::Error),
    #[error("Could not start {browser} for Overleaf sign-in: {source}")]
    Launch {
        browser: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{browser} closed before the Overleaf sign-in completed.")]
    BrowserClosed { browser: String },
    #[error("{browser} did not expose its private local sign-in window in time.")]
    DevToolsDidNotStart { browser: String },
    #[error("The private browser login ended before Semantic Zed could read its local session.")]
    DevToolsDisconnected,
    #[error(
        "The private browser login did not complete within {seconds} seconds. Finish the sign-in and retry."
    )]
    TimedOut { seconds: u64 },
    #[error(
        "The browser signed in, but no Overleaf cookies were available from the app-owned window."
    )]
    MissingCookies,
    #[error("The browser signed in, but the Overleaf session could not be validated: {0}")]
    SessionValidation(#[from] HttpError),
    #[error("Overleaf credential storage failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("The local browser sign-in protocol failed: {0}")]
    DevTools(String),
}

/// Sign in through a native, app-owned browser profile and persist the
/// validated Overleaf identity in the platform credential store.
///
/// On macOS this means the Keychain. Other platforms use the existing private
/// credential-file fallback. No personal Chrome, Arc, Safari, Firefox, or
/// system browser cookie store is inspected.
pub async fn authenticate_with_browser(
    server: &str,
) -> Result<CredentialRecord, BrowserLoginError> {
    authenticate_with_browser_with_store(server, &CredentialStore::default()).await
}

async fn authenticate_with_browser_with_store(
    server: &str,
    store: &CredentialStore,
) -> Result<CredentialRecord, BrowserLoginError> {
    let cookies = capture_browser_cookies(server, store, DEFAULT_LOGIN_TIMEOUT).await?;
    let mut client = OverleafHttpClient::new(server)?;
    let identity = client.login_with_cookies(&cookies).await?;
    Ok(store.save(server, identity)?)
}

async fn capture_browser_cookies(
    server: &str,
    store: &CredentialStore,
    timeout: Duration,
) -> Result<String, BrowserLoginError> {
    let server_url = canonical_server_url(server)?;
    let project_url = server_url.join("project")?;
    let profile = prepare_browser_profile(store, server)?;
    let arc_is_running = is_arc_running();
    let browser = find_browser_executable(arc_is_running).ok_or(
        BrowserLoginError::NoCompatibleBrowser {
            hint: if arc_is_running {
                "Arc is already open, so Semantic Zed will not start a second Arc instance. Install or choose Chrome, Brave, Edge, or Chromium, or close Arc and retry."
            } else {
                "Install Chrome, Brave, Edge, or Chromium, then retry."
            },
        },
    )?;
    let port = reserve_loopback_port()?;
    let mut browser_process = BrowserProcess::spawn(&browser, port, &profile, &project_url)?;
    let result = wait_for_login(&mut browser_process, port, &project_url, timeout).await;
    browser_process.terminate().await;
    result
}

fn prepare_browser_profile(
    store: &CredentialStore,
    server: &str,
) -> Result<PathBuf, BrowserLoginError> {
    let profile = store.browser_profile_path(server)?;
    fs::create_dir_all(&profile).map_err(BrowserLoginError::Profile)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700))
            .map_err(BrowserLoginError::Profile)?;
    }
    Ok(profile)
}

async fn wait_for_login(
    browser: &mut BrowserProcess,
    port: u16,
    project_url: &Url,
    timeout: Duration,
) -> Result<String, BrowserLoginError> {
    let deadline = Instant::now() + timeout;
    let target = wait_for_page_target(browser, port, project_url, deadline).await?;
    let (socket, _) = tokio::time::timeout(
        DEVTOOLS_REQUEST_TIMEOUT,
        connect_async(&target.web_socket_debugger_url),
    )
    .await
    .map_err(|_| {
        BrowserLoginError::DevTools("timed out connecting to the private browser window".into())
    })?
    .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
    let mut cdp = CdpClient::new(socket);

    while Instant::now() < deadline {
        browser.ensure_running()?;
        if let Some(state) = cdp.login_state().await? {
            if is_project_page(&state.href, project_url)
                && !state.user_id.is_empty()
                && !state.csrf.is_empty()
            {
                let cookies = cdp.cookies(project_url).await?;
                if cookies.is_empty() {
                    return Err(BrowserLoginError::MissingCookies);
                }
                let _ = cdp.close().await;
                return Ok(cookies);
            }
        }
        sleep(LOGIN_POLL_INTERVAL).await;
    }
    let _ = cdp.close().await;
    Err(BrowserLoginError::TimedOut {
        seconds: timeout.as_secs(),
    })
}

async fn wait_for_page_target(
    browser: &mut BrowserProcess,
    port: u16,
    project_url: &Url,
    deadline: Instant,
) -> Result<DevToolsTarget, BrowserLoginError> {
    let timeout_deadline = deadline.min(Instant::now() + DEVTOOLS_START_TIMEOUT);
    let client = reqwest::Client::builder()
        .timeout(DEVTOOLS_HTTP_TIMEOUT)
        .build()
        .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    while Instant::now() < timeout_deadline {
        browser.ensure_running()?;
        match client.get(&endpoint).send().await {
            Ok(response) if response.status().is_success() => {
                let body = response
                    .text()
                    .await
                    .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
                let targets = serde_json::from_str::<Vec<DevToolsTarget>>(&body)
                    .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
                let mut pages = targets.into_iter().filter(|target| {
                    target.kind == "page" && !target.web_socket_debugger_url.is_empty()
                });
                if let Some(target) = pages
                    .find(|target| is_project_page(&target.url, project_url))
                    .or_else(|| pages.next())
                {
                    return Ok(target);
                }
            }
            Ok(_) | Err(_) => {}
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(BrowserLoginError::DevToolsDidNotStart {
        browser: browser.name.clone(),
    })
}

fn reserve_loopback_port() -> Result<u16, BrowserLoginError> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(BrowserLoginError::ReservePort)?;
    let port = listener
        .local_addr()
        .map_err(BrowserLoginError::ReservePort)?
        .port();
    drop(listener);
    Ok(port)
}

struct BrowserProcess {
    name: String,
    child: Child,
    terminated: bool,
}

impl BrowserProcess {
    fn spawn(
        executable: &Path,
        port: u16,
        profile: &Path,
        project_url: &Url,
    ) -> Result<Self, BrowserLoginError> {
        let name = executable
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("browser")
            .to_string();
        let child = TokioCommand::new(executable)
            .args(browser_launch_arguments(port, profile, project_url))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Cancellation must not leave a private-profile browser alive.
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| BrowserLoginError::Launch {
                browser: name.clone(),
                source,
            })?;
        Ok(Self {
            name,
            child,
            terminated: false,
        })
    }

    fn ensure_running(&mut self) -> Result<(), BrowserLoginError> {
        if self
            .child
            .try_wait()
            .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?
            .is_some()
        {
            return Err(BrowserLoginError::BrowserClosed {
                browser: self.name.clone(),
            });
        }
        Ok(())
    }

    async fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.start_kill();
        }
        // `Child::wait` is asynchronous here. Never block the editor's Tokio
        // worker indefinitely while a browser is quitting.
        let _ = tokio::time::timeout(BROWSER_EXIT_TIMEOUT, self.child.wait()).await;
    }
}

/// Arguments are deliberately limited to a private profile and a loopback CDP
/// listener. In particular, do not add automation-evasion flags: they produce
/// browser security warnings and are not necessary for normal sign-in.
fn browser_launch_arguments(port: u16, profile: &Path, project_url: &Url) -> Vec<String> {
    vec![
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".into(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-features=Translate,PasswordManagerOnboarding".into(),
        "--new-window".into(),
        project_url.as_str().into(),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevToolsTarget {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    web_socket_debugger_url: String,
    #[serde(default)]
    url: String,
}

struct CdpClient<S> {
    socket: async_tungstenite::WebSocketStream<S>,
    next_id: u64,
}

impl<S> CdpClient<S>
where
    S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin,
{
    fn new(socket: async_tungstenite::WebSocketStream<S>) -> Self {
        Self { socket, next_id: 0 }
    }

    async fn login_state(&mut self) -> Result<Option<LoginPageState>, BrowserLoginError> {
        let response = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": "(() => ({ href: location.href, userId: document.querySelector('meta[name=\\\"ol-user_id\\\"]')?.content || '', csrf: document.querySelector('meta[name=\\\"ol-csrfToken\\\"]')?.content || '' }))()",
                    "returnByValue": true,
                }),
            )
            .await?;
        let value = response
            .get("result")
            .and_then(|value| value.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        if value.is_null() {
            return Ok(None);
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|error| BrowserLoginError::DevTools(error.to_string()))
    }

    async fn cookies(&mut self, project_url: &Url) -> Result<String, BrowserLoginError> {
        // Ask Chrome for only the Overleaf origin first. Older Chromium
        // builds can lack this CDP method, in which case the compatibility
        // fallback is filtered before it ever leaves this module.
        let result = match self
            .call(
                "Network.getCookies",
                json!({ "urls": [project_url.as_str()] }),
            )
            .await
        {
            Ok(result) => result,
            Err(_) => self.call("Storage.getCookies", json!({})).await?,
        };
        let cookies = result
            .get("cookies")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let cookies = serde_json::from_value::<Vec<BrowserCookie>>(cookies)
            .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
        Ok(cookies
            .into_iter()
            .filter(|cookie| !cookie.name.is_empty() && !cookie.value.is_empty())
            .filter(|cookie| cookie_applies(cookie, project_url))
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; "))
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, BrowserLoginError> {
        self.next_id += 1;
        let id = self.next_id;
        self.socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params })
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
        loop {
            let next = tokio::time::timeout(DEVTOOLS_REQUEST_TIMEOUT, self.socket.next())
                .await
                .map_err(|_| {
                    BrowserLoginError::DevTools(
                        "timed out waiting for the private browser window".into(),
                    )
                })?;
            let Some(message) = next else {
                return Err(BrowserLoginError::DevToolsDisconnected);
            };
            match message.map_err(|error| BrowserLoginError::DevTools(error.to_string()))? {
                Message::Text(payload) => {
                    let response = serde_json::from_str::<CdpResponse>(&payload)
                        .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
                    if response.id != Some(id) {
                        continue;
                    }
                    if let Some(error) = response.error {
                        return Err(BrowserLoginError::DevTools(error.message));
                    }
                    return Ok(response.result.unwrap_or(Value::Null));
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| BrowserLoginError::DevTools(error.to_string()))?;
                }
                Message::Close(_) => return Err(BrowserLoginError::DevToolsDisconnected),
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    async fn close(&mut self) -> Result<(), BrowserLoginError> {
        self.socket
            .close(None)
            .await
            .map_err(|error| BrowserLoginError::DevTools(error.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct CdpResponse {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<CdpError>,
}

#[derive(Debug, Deserialize)]
struct CdpError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct LoginPageState {
    #[serde(default)]
    href: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    csrf: String,
}

#[derive(Debug, Deserialize)]
struct BrowserCookie {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    path: String,
}

fn cookie_applies(cookie: &BrowserCookie, url: &Url) -> bool {
    let domain = cookie.domain.trim_start_matches('.');
    (domain.is_empty()
        || url
            .host_str()
            .is_some_and(|host| host == domain || host.ends_with(&format!(".{domain}"))))
        && (cookie.path.is_empty() || url.path().starts_with(&cookie.path))
}

fn is_project_page(current: &str, project_url: &Url) -> bool {
    let Ok(current) = Url::parse(current) else {
        return false;
    };
    current.origin() == project_url.origin() && current.path().starts_with(project_url.path())
}

fn find_browser_executable(arc_is_running: bool) -> Option<PathBuf> {
    browser_candidates(arc_is_running)
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| {
            browser_commands()
                .into_iter()
                .find_map(find_command_in_path)
        })
}

fn browser_candidates(arc_is_running: bool) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let default = mac_default_browser_executable();
        let installed = [
            "/Applications/Arc.app/Contents/MacOS/Arc",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ];
        let mut candidates = Vec::new();
        if let Some(default) = default {
            if !(arc_is_running && is_arc_executable(&default)) {
                candidates.push(default);
            }
        }
        for installed in installed {
            let candidate = PathBuf::from(installed);
            if !(arc_is_running && is_arc_executable(&candidate))
                && !candidates.contains(&candidate)
            {
                candidates.push(candidate);
            }
        }
        candidates
    }
    #[cfg(target_os = "windows")]
    {
        let _ = arc_is_running;
        let program_files = std::env::var_os("PROGRAMFILES").unwrap_or_default();
        let program_files_x86 = std::env::var_os("PROGRAMFILES(X86)").unwrap_or_default();
        let local_app_data = std::env::var_os("LOCALAPPDATA").unwrap_or_default();
        vec![
            PathBuf::from(&program_files).join("Google/Chrome/Application/chrome.exe"),
            PathBuf::from(&program_files_x86).join("Google/Chrome/Application/chrome.exe"),
            PathBuf::from(&program_files).join("Microsoft/Edge/Application/msedge.exe"),
            PathBuf::from(&local_app_data).join("Google/Chrome/Application/chrome.exe"),
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = arc_is_running;
        Vec::new()
    }
}

fn browser_commands() -> Vec<&'static str> {
    #[cfg(target_os = "windows")]
    {
        return vec!["chrome.exe", "msedge.exe", "chromium.exe"];
    }
    #[cfg(target_os = "macos")]
    {
        return Vec::new();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        vec![
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ]
    }
}

fn find_command_in_path(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(command))
        .find(|candidate| candidate.is_file())
}

#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_methods)]
fn is_arc_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", "Arc"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(target_os = "macos"))]
fn is_arc_running() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn is_arc_executable(executable: &Path) -> bool {
    executable == Path::new("/Applications/Arc.app/Contents/MacOS/Arc")
}

#[cfg(not(target_os = "macos"))]
fn is_arc_executable(_: &Path) -> bool {
    false
}

#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_methods)]
fn mac_default_browser_executable() -> Option<PathBuf> {
    let output = Command::new("/usr/bin/defaults")
        .args([
            "read",
            "com.apple.LaunchServices/com.apple.launchservices.secure",
            "LSHandlers",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let plist = String::from_utf8_lossy(&output.stdout);
    let bundle_id = https_handler_bundle_id(&plist)?;
    match bundle_id.as_str() {
        "company.thebrowser.Browser" | "company.thebrowser.browser" => {
            Some(PathBuf::from("/Applications/Arc.app/Contents/MacOS/Arc"))
        }
        "com.google.Chrome" => Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        "com.brave.Browser" => Some(PathBuf::from(
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        )),
        "com.microsoft.edgemac" => Some(PathBuf::from(
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        )),
        "org.chromium.Chromium" => Some(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        )),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn https_handler_bundle_id(plist: &str) -> Option<String> {
    let mut block = String::new();
    for line in plist.lines() {
        block.push_str(line);
        block.push('\n');
        if line.trim() == "}," || line.trim() == "}" {
            if block.contains("LSHandlerURLScheme = https;") {
                let marker = "LSHandlerRoleAll = \"";
                if let Some(start) = block.find(marker) {
                    let remainder = &block[start + marker.len()..];
                    if let Some(end) = remainder.find('"') {
                        return Some(remainder[..end].to_string());
                    }
                }
            }
            block.clear();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_login_arguments_use_only_a_private_loopback_profile() {
        let args = browser_launch_arguments(
            4242,
            Path::new("/private/profile"),
            &Url::parse("https://www.overleaf.com/project").unwrap(),
        );
        assert!(args.contains(&"--remote-debugging-address=127.0.0.1".to_string()));
        assert!(args.contains(&"--user-data-dir=/private/profile".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("AutomationControlled")));
        assert!(!args.iter().any(|arg| arg.contains("--profile-directory")));
    }

    #[test]
    fn cookies_are_limited_to_the_authenticated_overleaf_origin() {
        let url = Url::parse("https://www.overleaf.com/project").unwrap();
        assert!(cookie_applies(
            &BrowserCookie {
                name: "session".into(),
                value: "secret".into(),
                domain: ".overleaf.com".into(),
                path: "/".into(),
            },
            &url,
        ));
        assert!(!cookie_applies(
            &BrowserCookie {
                name: "other".into(),
                value: "secret".into(),
                domain: "example.com".into(),
                path: "/".into(),
            },
            &url,
        ));
    }

    #[test]
    fn project_page_check_rejects_identity_provider_redirects() {
        let project_url = Url::parse("https://www.overleaf.com/project").unwrap();
        assert!(is_project_page(
            "https://www.overleaf.com/project/abc123",
            &project_url
        ));
        assert!(!is_project_page(
            "https://accounts.google.com/signin",
            &project_url
        ));
    }

    #[test]
    fn app_owned_profile_uses_the_credential_store_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::default().with_app_data_dir(directory.path());
        let profile = prepare_browser_profile(&store, "https://www.overleaf.com").unwrap();
        assert!(profile.starts_with(directory.path()));
        assert!(profile.ends_with("browser-profiles/aHR0cHM6Ly93d3cub3ZlcmxlYWYuY29tLw"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(profile).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_the_https_default_browser_bundle() {
        let plist = r#"(
            {
                LSHandlerURLScheme = https;
                LSHandlerRoleAll = "company.thebrowser.Browser";
            }
        )"#;
        assert_eq!(
            https_handler_bundle_id(plist).as_deref(),
            Some("company.thebrowser.Browser")
        );
    }
}
