use std::collections::HashMap;
use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{
    CONTENT_RANGE, CONTENT_TYPE, COOKIE, HeaderMap, LOCATION, RANGE, SET_COOKIE,
};
use reqwest::{Method, StatusCode, Url, multipart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const COMPILE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_DOWNLOAD_REQUESTS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub cookies: String,
    pub csrf_token: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_email: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid Overleaf server URL: {0}")]
    InvalidServer(#[from] url::ParseError),
    #[error("Overleaf identity is not configured")]
    MissingIdentity,
    #[error("Overleaf session is no longer valid for {server} ({status})")]
    SessionExpired { server: String, status: u16 },
    #[error("Overleaf authentication page did not expose {0}")]
    MissingLoginMetadata(&'static str),
    #[error("Overleaf request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid Overleaf response: {0}")]
    InvalidResponse(String),
    #[error("{method} {route} failed ({status}): {body}")]
    Status {
        method: Method,
        route: String,
        status: StatusCode,
        body: String,
    },
    #[error("Overleaf download returned a non-contiguous range: {0}")]
    NonContiguousRange(String),
    #[error("Overleaf download made no progress: {0}")]
    DownloadStalled(String),
    #[error("Overleaf download did not complete after {MAX_DOWNLOAD_REQUESTS} requests: {0}")]
    DownloadIncomplete(String),
}

pub struct OverleafHttpClient {
    base_url: Url,
    client: reqwest::Client,
    identity: Option<Identity>,
}

impl OverleafHttpClient {
    pub fn new(server_url: &str) -> Result<Self, HttpError> {
        let base_url = canonical_server_url(server_url)?;
        let client = reqwest::Client::builder()
            .redirect_policy(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            base_url,
            client,
            identity: None,
        })
    }

    pub fn origin(&self) -> String {
        self.base_url.origin().ascii_serialization()
    }

    pub fn server_url(&self) -> &Url {
        &self.base_url
    }

    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    pub fn set_identity(&mut self, identity: Identity) -> &mut Self {
        self.identity = Some(identity);
        self
    }

    pub fn take_identity(&mut self) -> Option<Identity> {
        self.identity.take()
    }

    pub async fn login_with_cookies(&mut self, cookies: &str) -> Result<Identity, HttpError> {
        let project_url = self.route("project")?;
        let project_response = self
            .client
            .get(project_url)
            .header(COOKIE, cookies)
            .send()
            .await?;
        let project_status = project_response.status();
        let project_headers = project_response.headers().clone();
        let body = project_response.text().await?;
        self.check_session(project_status, &project_headers, &body)?;

        let user_id = extract_meta_content(&body, "ol-user_id")
            .filter(|value| !value.is_empty())
            .ok_or(HttpError::MissingLoginMetadata("ol-user_id"))?;
        let csrf_token = extract_meta_content(&body, "ol-csrfToken")
            .filter(|value| !value.is_empty())
            .ok_or(HttpError::MissingLoginMetadata("ol-csrfToken"))?;
        let user_email = extract_meta_content(&body, "ol-usersEmail").unwrap_or_default();

        let socket_url = self.route("socket.io/socket.io.js")?;
        let socket_response = self
            .client
            .get(socket_url)
            .header(COOKIE, cookies)
            .send()
            .await?;
        let merged_cookies =
            merge_cookie_header(cookies, &set_cookie_values(socket_response.headers()));
        let identity = Identity {
            cookies: merged_cookies,
            csrf_token,
            user_id,
            user_email,
        };
        self.identity = Some(identity.clone());
        Ok(identity)
    }

    pub async fn list_projects(&mut self) -> Result<Vec<Value>, HttpError> {
        let primary = self
            .request(
                Method::POST,
                "api/project",
                Some(json!({})),
                false,
                REQUEST_TIMEOUT,
            )
            .await?;
        let response = if primary.is_success() {
            primary
        } else {
            self.request(Method::GET, "user/projects", None, false, REQUEST_TIMEOUT)
                .await?
        };
        if !response.is_success() {
            return Err(HttpError::Status {
                method: Method::GET,
                route: "user/projects".into(),
                status: response.status,
                body: response.body,
            });
        }
        let value: Value = serde_json::from_str(&response.body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
        let mut projects = value
            .get("projects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for project in &mut projects {
            let Some(object) = project.as_object_mut() else {
                continue;
            };
            if !object.contains_key("id")
                && let Some(id) = object.get("_id").cloned()
            {
                object.insert("id".into(), id);
            }
        }
        Ok(projects)
    }

    pub async fn create_project(
        &mut self,
        project_name: &str,
        template: &str,
    ) -> Result<Value, HttpError> {
        let project_name = project_name.trim();
        if project_name.is_empty() {
            return Err(HttpError::InvalidResponse(
                "a project name is required".into(),
            ));
        }
        if !matches!(template, "none" | "example") {
            return Err(HttpError::InvalidResponse(format!(
                "unsupported project template: {template}"
            )));
        }
        let response = self
            .request(
                Method::POST,
                "project/new",
                Some(json!({ "projectName": project_name, "template": template })),
                false,
                REQUEST_TIMEOUT,
            )
            .await?;
        self.require_success(Method::POST, "project/new", response)
            .and_then(|response| {
                let value: Value = serde_json::from_str(&response.body)
                    .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
                let id = value
                    .get("project_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| HttpError::InvalidResponse("missing project_id".into()))?;
                Ok(json!({ "id": id, "name": project_name, "template": template }))
            })
    }

    pub async fn trash_project(&mut self, project_id: &str) -> Result<(), HttpError> {
        let project_id = validated_project_id(project_id)?;
        let route = format!("project/{project_id}/trash");
        let response = self
            .request(Method::POST, &route, None, true, REQUEST_TIMEOUT)
            .await?;
        self.require_success(Method::POST, &route, response)?;
        Ok(())
    }

    pub async fn untrash_project(&mut self, project_id: &str) -> Result<(), HttpError> {
        let project_id = validated_project_id(project_id)?;
        let route = format!("project/{project_id}/trash");
        let response = self
            .request(Method::DELETE, &route, None, true, REQUEST_TIMEOUT)
            .await?;
        self.require_success(Method::DELETE, &route, response)?;
        Ok(())
    }

    pub async fn get_project_entities(
        &mut self,
        project_id: &str,
    ) -> Result<Vec<Value>, HttpError> {
        let route = format!("project/{}/entities", path_segment(project_id));
        let response = self
            .request(Method::GET, &route, None, false, REQUEST_TIMEOUT)
            .await?;
        let response = self.require_success(Method::GET, &route, response)?;
        let value: Value = serde_json::from_str(&response.body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
        Ok(value
            .get("entities")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn download_document(
        &mut self,
        project_id: &str,
        document_id: &str,
    ) -> Result<Vec<u8>, HttpError> {
        self.download(&format!(
            "project/{}/doc/{}/download",
            path_segment(project_id),
            path_segment(document_id)
        ))
        .await
    }

    pub async fn download_file(
        &mut self,
        project_id: &str,
        file_id: &str,
    ) -> Result<Vec<u8>, HttpError> {
        self.download(&format!(
            "project/{}/file/{}",
            path_segment(project_id),
            path_segment(file_id)
        ))
        .await
    }

    pub async fn add_document(
        &mut self,
        project_id: &str,
        parent_folder_id: &str,
        name: &str,
    ) -> Result<Value, HttpError> {
        let route = format!("project/{}/doc", path_segment(project_id));
        let response = self
            .request(
                Method::POST,
                &route,
                Some(json!({ "parent_folder_id": parent_folder_id, "name": name })),
                true,
                REQUEST_TIMEOUT,
            )
            .await?;
        let response = self.require_success(Method::POST, &route, response)?;
        let value: Value = serde_json::from_str(&response.body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
        Ok(json!({
            "_id": value.get("_id").cloned().unwrap_or(Value::Null),
            "name": name,
            "_type": "doc"
        }))
    }

    pub async fn add_folder(
        &mut self,
        project_id: &str,
        parent_folder_id: &str,
        name: &str,
    ) -> Result<Value, HttpError> {
        let route = format!("project/{}/folder", path_segment(project_id));
        let response = self
            .request(
                Method::POST,
                &route,
                Some(json!({ "parent_folder_id": parent_folder_id, "name": name })),
                true,
                REQUEST_TIMEOUT,
            )
            .await?;
        let response = self.require_success(Method::POST, &route, response)?;
        let mut value: Value = serde_json::from_str(&response.body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("_type".into(), Value::String("folder".into()));
        }
        Ok(value)
    }

    pub async fn upload_file(
        &mut self,
        project_id: &str,
        parent_folder_id: &str,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<Value, HttpError> {
        let identity = self.require_identity()?.clone();
        let mime = guess_mime(name);
        let file_part = multipart::Part::bytes(bytes)
            .file_name(name.to_owned())
            .mime_str(mime)
            .map_err(HttpError::Request)?;
        let form = multipart::Form::new()
            .text("targetFolderId", parent_folder_id.to_owned())
            .text("name", name.to_owned())
            .text("type", mime.to_owned())
            .part("qqfile", file_part);
        let route = format!(
            "project/{}/upload?folder_id={}",
            path_segment(project_id),
            path_segment(parent_folder_id)
        );
        let url = self.route(&route)?;
        let response = self
            .client
            .post(url)
            .header(COOKIE, &identity.cookies)
            .header("X-Csrf-Token", &identity.csrf_token)
            .timeout(DOWNLOAD_TIMEOUT)
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let headers = response.headers().clone();
        self.merge_response_cookies(&headers);
        let body = response.text().await?;
        self.check_session(status, &headers, &body)?;
        if !status.is_success() {
            return Err(HttpError::Status {
                method: Method::POST,
                route,
                status,
                body,
            });
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(HttpError::InvalidResponse(body));
        }
        let entity_id = value
            .get("entity_id")
            .and_then(Value::as_str)
            .ok_or_else(|| HttpError::InvalidResponse("upload response has no entity_id".into()))?;
        Ok(json!({
            "_id": entity_id,
            "_type": value.get("entity_type").and_then(Value::as_str).unwrap_or("file"),
            "name": name,
            "linkedFileData": Value::Null
        }))
    }

    pub async fn delete_entity(
        &mut self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), HttpError> {
        let route = format!(
            "project/{}/{}/{}",
            path_segment(project_id),
            path_segment(entity_type),
            path_segment(entity_id)
        );
        let response = self
            .request(Method::DELETE, &route, None, true, REQUEST_TIMEOUT)
            .await?;
        self.require_success(Method::DELETE, &route, response)?;
        Ok(())
    }

    pub async fn rename_entity(
        &mut self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
        name: &str,
    ) -> Result<(), HttpError> {
        let route = format!(
            "project/{}/{}/{}/rename",
            path_segment(project_id),
            path_segment(entity_type),
            path_segment(entity_id)
        );
        let response = self
            .request(
                Method::POST,
                &route,
                Some(json!({ "name": name })),
                true,
                REQUEST_TIMEOUT,
            )
            .await?;
        self.require_success(Method::POST, &route, response)?;
        Ok(())
    }

    pub async fn move_entity(
        &mut self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
        parent_folder_id: &str,
    ) -> Result<(), HttpError> {
        let route = format!(
            "project/{}/{}/{}/move",
            path_segment(project_id),
            path_segment(entity_type),
            path_segment(entity_id)
        );
        let response = self
            .request(
                Method::POST,
                &route,
                Some(json!({ "folder_id": parent_folder_id })),
                true,
                REQUEST_TIMEOUT,
            )
            .await?;
        self.require_success(Method::POST, &route, response)?;
        Ok(())
    }

    pub async fn compile(
        &mut self,
        project_id: &str,
        root_resource_path: Option<&str>,
    ) -> Result<Value, HttpError> {
        let route = format!(
            "project/{}/compile?auto_compile=true",
            path_segment(project_id)
        );
        let response = self
            .request(
                Method::POST,
                &route,
                Some(json!({
                    "check": "silent",
                    "draft": false,
                    "incrementalCompilesEnabled": true,
                    "rootResourcePath": root_resource_path,
                    "stopOnFirstError": false
                })),
                true,
                COMPILE_TIMEOUT,
            )
            .await?;
        let response = self.require_success(Method::POST, &route, response)?;
        serde_json::from_str(&response.body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))
    }

    pub async fn download_compile_output(
        &mut self,
        compile: &Value,
        name: &str,
    ) -> Result<Vec<u8>, HttpError> {
        let output = compile
            .get("outputFiles")
            .and_then(Value::as_array)
            .and_then(|outputs| {
                outputs.iter().find(|output| {
                    output.get("path").and_then(Value::as_str) == Some(name)
                        || output.get("name").and_then(Value::as_str) == Some(name)
                        || output
                            .get("url")
                            .and_then(Value::as_str)
                            .is_some_and(|url| url.ends_with(&format!("/{name}")))
                })
            })
            .ok_or_else(|| HttpError::InvalidResponse(format!("missing compile output: {name}")))?;
        let output_url = output
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| HttpError::InvalidResponse("compile output has no URL".into()))?;
        if let (Some(domain), Some(clsi_server_id)) = (
            compile.get("pdfDownloadDomain").and_then(Value::as_str),
            compile.get("clsiServerId").and_then(Value::as_str),
        ) {
            let mut url = Url::parse(&format!(
                "{}/{}",
                domain.trim_end_matches('/'),
                output_url.trim_start_matches('/')
            ))?;
            url.query_pairs_mut()
                .append_pair(
                    "compileGroup",
                    compile
                        .get("compileGroup")
                        .and_then(Value::as_str)
                        .unwrap_or("standard"),
                )
                .append_pair("clsiserverid", clsi_server_id)
                .append_pair("enable_pdf_caching", "true");
            return self.download_absolute(url, false).await;
        }
        self.download(output_url.trim_start_matches('/')).await
    }

    pub async fn sync_code(
        &mut self,
        project_id: &str,
        file: &str,
        line: u64,
        column: u64,
        build_id: Option<&str>,
        clsi_server_id: Option<&str>,
        editor_id: &str,
    ) -> Result<Vec<Value>, HttpError> {
        let mut url = self.route(&format!("project/{}/sync/code", path_segment(project_id)))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("file", file)
                .append_pair("line", &line.to_string())
                .append_pair("column", &column.to_string())
                .append_pair("editorId", editor_id);
            if let Some(build_id) = build_id {
                query.append_pair("buildId", build_id);
            }
            if let Some(clsi_server_id) = clsi_server_id {
                query.append_pair("clsiserverid", clsi_server_id);
            }
        }
        self.sync_request(url, "pdf").await
    }

    pub async fn sync_pdf(
        &mut self,
        project_id: &str,
        page: u64,
        horizontal: f64,
        vertical: f64,
        build_id: Option<&str>,
        clsi_server_id: Option<&str>,
        editor_id: &str,
    ) -> Result<Vec<Value>, HttpError> {
        let mut url = self.route(&format!("project/{}/sync/pdf", path_segment(project_id)))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("page", &page.to_string())
                .append_pair("h", &format!("{horizontal:.2}"))
                .append_pair("v", &format!("{vertical:.2}"))
                .append_pair("editorId", editor_id);
            if let Some(build_id) = build_id {
                query.append_pair("buildId", build_id);
            }
            if let Some(clsi_server_id) = clsi_server_id {
                query.append_pair("clsiserverid", clsi_server_id);
            }
        }
        self.sync_request(url, "code").await
    }

    pub async fn get_messages(
        &mut self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>, HttpError> {
        let route = format!(
            "project/{}/messages?limit={limit}",
            path_segment(project_id)
        );
        let response = self
            .request(Method::GET, &route, None, false, REQUEST_TIMEOUT)
            .await?;
        let response = self.require_success(Method::GET, &route, response)?;
        serde_json::from_str(&response.body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))
    }

    pub async fn send_message(
        &mut self,
        project_id: &str,
        client_id: &str,
        content: &str,
    ) -> Result<(), HttpError> {
        let route = format!("project/{}/messages", path_segment(project_id));
        let response = self
            .request(
                Method::POST,
                &route,
                Some(json!({ "client_id": client_id, "content": content })),
                true,
                REQUEST_TIMEOUT,
            )
            .await?;
        self.require_success(Method::POST, &route, response)?;
        Ok(())
    }

    pub async fn request(
        &mut self,
        method: Method,
        route: &str,
        body: Option<Value>,
        csrf_header: bool,
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let identity = self.require_identity()?.clone();
        let url = self.route(route)?;
        let mut request = self
            .client
            .request(method.clone(), url)
            .header(COOKIE, &identity.cookies)
            .timeout(timeout);
        if csrf_header || method == Method::DELETE {
            request = request.header("X-Csrf-Token", &identity.csrf_token);
        }
        if let Some(body) = body {
            let mut object = body.as_object().cloned().ok_or_else(|| {
                HttpError::InvalidResponse("request JSON body must be an object".into())
            })?;
            object.insert("_csrf".into(), Value::String(identity.csrf_token.clone()));
            let bytes = serde_json::to_vec(&object)
                .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
            request = request.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let response = request.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        self.merge_response_cookies(&headers);
        let body = if status == StatusCode::NO_CONTENT {
            String::new()
        } else {
            response.text().await?
        };
        self.check_session(status, &headers, &body)?;
        Ok(HttpResponse { status, body })
    }

    pub async fn download(&mut self, route: &str) -> Result<Vec<u8>, HttpError> {
        let url = self.route(route)?;
        let mut chunks = Vec::new();
        let mut received = 0usize;
        for _ in 0..MAX_DOWNLOAD_REQUESTS {
            let cookies = self.require_identity()?.cookies.clone();
            let mut request = self
                .client
                .get(url.clone())
                .header(COOKIE, cookies)
                .timeout(DOWNLOAD_TIMEOUT);
            if received > 0 {
                request = request.header(RANGE, format!("bytes={received}-"));
            }
            let response = request.send().await?;
            let status = response.status();
            let headers = response.headers().clone();
            self.merge_response_cookies(&headers);
            if status == StatusCode::OK {
                return Ok(response.bytes().await?.to_vec());
            }
            if status == StatusCode::PARTIAL_CONTENT {
                let range = headers
                    .get(CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let bytes = response.bytes().await?.to_vec();
                if bytes.is_empty() {
                    return Err(HttpError::DownloadStalled(route.into()));
                }
                let parsed = range.as_deref().and_then(parse_content_range);
                if let Some((start, _, _)) = parsed
                    && start != received
                {
                    return Err(HttpError::NonContiguousRange(range.unwrap_or_default()));
                }
                received += bytes.len();
                chunks.extend(bytes);
                if parsed
                    .and_then(|(_, _, total)| total)
                    .is_some_and(|total| received >= total)
                {
                    return Ok(chunks);
                }
                continue;
            }
            if status == StatusCode::RANGE_NOT_SATISFIABLE && received > 0 {
                return Ok(chunks);
            }
            let body = response.text().await?;
            self.check_session(status, &headers, &body)?;
            return Err(HttpError::Status {
                method: Method::GET,
                route: route.into(),
                status,
                body,
            });
        }
        Err(HttpError::DownloadIncomplete(route.into()))
    }

    pub async fn download_absolute(
        &mut self,
        url: Url,
        include_cookies: bool,
    ) -> Result<Vec<u8>, HttpError> {
        let mut request = self.client.get(url.clone()).timeout(DOWNLOAD_TIMEOUT);
        if include_cookies {
            request = request.header(COOKIE, &self.require_identity()?.cookies);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(HttpError::Status {
                method: Method::GET,
                route: url.to_string(),
                status,
                body: response.text().await.unwrap_or_default(),
            });
        }
        Ok(response.bytes().await?.to_vec())
    }

    async fn sync_request(&mut self, url: Url, key: &str) -> Result<Vec<Value>, HttpError> {
        let identity = self.require_identity()?.clone();
        let response = self
            .client
            .get(url.clone())
            .header(COOKIE, &identity.cookies)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        let status = response.status();
        let headers = response.headers().clone();
        self.merge_response_cookies(&headers);
        let body = response.text().await?;
        self.check_session(status, &headers, &body)?;
        if !status.is_success() {
            return Err(HttpError::Status {
                method: Method::GET,
                route: url.to_string(),
                status,
                body,
            });
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| HttpError::InvalidResponse(error.to_string()))?;
        Ok(value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn route(&self, route: &str) -> Result<Url, HttpError> {
        if route.starts_with("http://") || route.starts_with("https://") {
            Ok(Url::parse(route)?)
        } else {
            Ok(self.base_url.join(route.trim_start_matches('/'))?)
        }
    }

    fn require_identity(&self) -> Result<&Identity, HttpError> {
        self.identity
            .as_ref()
            .filter(|identity| !identity.cookies.is_empty() && !identity.csrf_token.is_empty())
            .ok_or(HttpError::MissingIdentity)
    }

    fn require_success(
        &self,
        method: Method,
        route: &str,
        response: HttpResponse,
    ) -> Result<HttpResponse, HttpError> {
        if response.is_success() {
            Ok(response)
        } else {
            Err(HttpError::Status {
                method,
                route: route.into(),
                status: response.status,
                body: response.body,
            })
        }
    }

    fn merge_response_cookies(&mut self, headers: &HeaderMap) {
        let values = set_cookie_values(headers);
        if values.is_empty() {
            return;
        }
        if let Some(identity) = &mut self.identity {
            identity.cookies = merge_cookie_header(&identity.cookies, &values);
        }
    }

    fn check_session(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &str,
    ) -> Result<(), HttpError> {
        let location = headers
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let redirect_to_login = matches!(
            status,
            StatusCode::FOUND | StatusCode::SEE_OTHER | StatusCode::TEMPORARY_REDIRECT
        ) && (location.ends_with("/login")
            || location.contains("/login?")
            || location.contains("/login#"));
        let body_lower = body.to_ascii_lowercase();
        let invalid_csrf = status == StatusCode::FORBIDDEN
            && (body_lower.contains("ebadcsrftoken") || body_lower.contains("invalid csrf token"));
        if status == StatusCode::UNAUTHORIZED || redirect_to_login || invalid_csrf {
            return Err(HttpError::SessionExpired {
                server: self.base_url.to_string(),
                status: status.as_u16(),
            });
        }
        Ok(())
    }
}

pub fn canonical_server_url(server: &str) -> Result<Url, url::ParseError> {
    let mut url = Url::parse(server)?;
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

pub fn split_set_cookie_header(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut values = Vec::new();
    let mut start = 0;
    for (index, character) in value.char_indices() {
        if character != ',' {
            continue;
        }
        let tail = &value[index + 1..];
        let candidate = tail.trim_start();
        let boundary = candidate.find([';', ',']).unwrap_or(candidate.len());
        let token = &candidate[..boundary];
        if token
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && !name.chars().any(char::is_whitespace))
        {
            values.push(value[start..index].to_owned());
            start = index + 1;
        }
    }
    values.push(value[start..].to_owned());
    values
}

pub fn merge_cookie_header(existing: &str, set_cookies: &[String]) -> String {
    let mut cookies = Vec::<(String, String)>::new();
    for value in existing
        .split(';')
        .map(str::trim)
        .chain(set_cookies.iter().map(|value| value.as_str()))
    {
        let Some(pair) = value.split(';').next() else {
            continue;
        };
        let Some((name, cookie_value)) = pair.trim().split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if let Some(existing) = cookies.iter_mut().find(|(candidate, _)| candidate == name) {
            existing.1 = cookie_value.trim().into();
        } else {
            cookies.push((name.trim().into(), cookie_value.trim().into()));
        }
    }
    cookies
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn set_cookie_values(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(split_set_cookie_header)
        .collect()
}

fn extract_meta_content(html: &str, desired_name: &str) -> Option<String> {
    let mut cursor = 0;
    while let Some(relative_start) = html[cursor..].find("<meta") {
        let start = cursor + relative_start;
        let end = html[start..].find('>')? + start;
        let tag = &html[start..=end];
        let attributes = parse_html_attributes(tag);
        if attributes.get("name").map(String::as_str) == Some(desired_name) {
            return attributes.get("content").cloned();
        }
        cursor = end + 1;
    }
    None
}

fn parse_html_attributes(tag: &str) -> HashMap<String, String> {
    let bytes = tag.as_bytes();
    let mut attributes = HashMap::new();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'-' | b'_'))
        {
            index += 1;
        }
        if name_start == index {
            break;
        }
        let name = tag[name_start..index].to_ascii_lowercase();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let Some(quote @ (b'\'' | b'"')) = bytes.get(index).copied() else {
            continue;
        };
        index += 1;
        let value_start = index;
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        if index <= bytes.len() {
            attributes.insert(name, tag[value_start..index].to_owned());
        }
        index += 1;
    }
    attributes
}

fn path_segment(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn validated_project_id(project_id: &str) -> Result<String, HttpError> {
    if project_id.trim().is_empty()
        || project_id != project_id.trim()
        || project_id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || project_id.contains(['/', '\\', '?', '#'])
    {
        return Err(HttpError::InvalidResponse(
            "project id must be a non-empty route segment".into(),
        ));
    }
    Ok(path_segment(project_id))
}

fn parse_content_range(value: &str) -> Option<(usize, usize, Option<usize>)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((
        start.parse().ok()?,
        end.parse().ok()?,
        (total != "*").then(|| total.parse().ok()).flatten(),
    ))
}

fn guess_mime(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "eps" => "application/postscript",
        "tex" | "bib" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    use super::*;

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static [u8],
    }

    fn mock_server(
        responses: Vec<MockResponse>,
    ) -> (String, Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                let header_end = loop {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break request.len();
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(position) =
                        request.windows(4).position(|value| value == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                sender
                    .send(String::from_utf8_lossy(&request).into_owned())
                    .unwrap();
                let reason = match response.status {
                    200 => "OK",
                    206 => "Partial Content",
                    302 => "Found",
                    401 => "Unauthorized",
                    _ => "Response",
                };
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    reason,
                    response.body.len()
                );
                for (name, value) in response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(response.body).unwrap();
            }
        });
        (format!("http://{address}/"), receiver, handle)
    }

    fn identity() -> Identity {
        Identity {
            cookies: "session=x".into(),
            csrf_token: "csrf".into(),
            user_id: "user-1".into(),
            user_email: "person@example.com".into(),
        }
    }

    #[test]
    fn splits_combined_set_cookie_without_splitting_expires() {
        assert_eq!(
            split_set_cookie_header(
                "a=1; Expires=Wed, 21 Oct 2030 07:28:00 GMT; Path=/, b=2; Path=/"
            ),
            vec![
                "a=1; Expires=Wed, 21 Oct 2030 07:28:00 GMT; Path=/".to_string(),
                " b=2; Path=/".to_string()
            ]
        );
        assert_eq!(
            merge_cookie_header("a=old; keep=yes", &["a=new; Path=/".into(), "b=2".into()]),
            "a=new; keep=yes; b=2"
        );
    }

    #[tokio::test]
    async fn login_reuses_cookie_session_and_merges_socket_cookie() {
        let (url, requests, server) = mock_server(vec![
            MockResponse {
                status: 200,
                headers: vec![],
                body: br#"<meta content="user-1" name="ol-user_id"><meta name="ol-usersEmail" content="person@example.com"><meta name="ol-csrfToken" content="csrf-1">"#,
            },
            MockResponse {
                status: 200,
                headers: vec![("Set-Cookie", "socket=ready; Path=/")],
                body: b"bootstrap",
            },
        ]);
        let mut client = OverleafHttpClient::new(&url).unwrap();
        let identity = client.login_with_cookies("session=active").await.unwrap();
        assert_eq!(identity.user_id, "user-1");
        assert_eq!(identity.user_email, "person@example.com");
        assert_eq!(identity.csrf_token, "csrf-1");
        assert_eq!(identity.cookies, "session=active; socket=ready");
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /project HTTP/1.1")
        );
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /socket.io/socket.io.js HTTP/1.1")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn list_and_create_projects_match_dashboard_contract() {
        let (url, requests, server) = mock_server(vec![
            MockResponse {
                status: 200,
                headers: vec![],
                body: br#"{"projects":[{"_id":"paper-1","name":"Newest","lastUpdated":"2026-08-09"}]}"#,
            },
            MockResponse {
                status: 200,
                headers: vec![],
                body: br#"{"project_id":"paper-2"}"#,
            },
        ]);
        let mut client = OverleafHttpClient::new(&url).unwrap();
        client.set_identity(identity());
        let projects = client.list_projects().await.unwrap();
        assert_eq!(projects[0]["id"], "paper-1");
        let created = client.create_project("A new paper", "none").await.unwrap();
        assert_eq!(created["id"], "paper-2");

        let list_request = requests.recv().unwrap();
        assert!(list_request.starts_with("POST /api/project HTTP/1.1"));
        let create_request = requests.recv().unwrap();
        assert!(create_request.starts_with("POST /project/new HTTP/1.1"));
        let body = create_request.split("\r\n\r\n").nth(1).unwrap();
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["_csrf"], "csrf");
        assert_eq!(value["projectName"], "A new paper");
        assert_eq!(value["template"], "none");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn trash_and_untrash_projects_use_recoverable_dashboard_routes() {
        let project_id = "66abc0da96a34861ab376243";
        let (url, requests, server) = mock_server(vec![
            MockResponse {
                status: 200,
                headers: vec![],
                body: b"",
            },
            MockResponse {
                status: 204,
                headers: vec![],
                body: b"",
            },
        ]);
        let mut client = OverleafHttpClient::new(&url).unwrap();
        client.set_identity(identity());

        client.trash_project(project_id).await.unwrap();
        client.untrash_project(project_id).await.unwrap();

        let trash_request = requests.recv().unwrap();
        assert!(
            trash_request.starts_with(&format!("POST /project/{project_id}/trash HTTP/1.1")),
            "{trash_request}"
        );
        assert!(
            trash_request
                .to_ascii_lowercase()
                .contains("cookie: session=x\r\n")
        );
        assert!(
            trash_request
                .to_ascii_lowercase()
                .contains("x-csrf-token: csrf\r\n")
        );

        let untrash_request = requests.recv().unwrap();
        assert!(
            untrash_request.starts_with(&format!("DELETE /project/{project_id}/trash HTTP/1.1")),
            "{untrash_request}"
        );
        assert!(
            untrash_request
                .to_ascii_lowercase()
                .contains("x-csrf-token: csrf\r\n")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn trash_project_surfaces_server_failure() {
        let project_id = "66abc0da96a34861ab376243";
        let expected_route = format!("project/{project_id}/trash");
        let (url, _requests, server) = mock_server(vec![MockResponse {
            status: 500,
            headers: vec![],
            body: b"trash unavailable",
        }]);
        let mut client = OverleafHttpClient::new(&url).unwrap();
        client.set_identity(identity());

        let error = client.trash_project(project_id).await.unwrap_err();
        assert!(
            matches!(
                error,
                HttpError::Status {
                    method: Method::POST,
                    ref route,
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    ref body,
                } if route == &expected_route && body == "trash unavailable"
            ),
            "{error:?}"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn trash_project_rejects_blank_or_unsafe_project_ids_before_request() {
        let mut client = OverleafHttpClient::new("http://127.0.0.1:9/").unwrap();
        client.set_identity(identity());

        for project_id in [
            "",
            "  ",
            "paper/id",
            "../paper",
            "paper?copy",
            " paper",
            "paper\0id",
        ] {
            let error = client.trash_project(project_id).await.unwrap_err();
            assert!(matches!(error, HttpError::InvalidResponse(_)));
        }
    }

    #[tokio::test]
    async fn continues_partial_download_with_contiguous_range() {
        let (url, requests, server) = mock_server(vec![
            MockResponse {
                status: 206,
                headers: vec![("Content-Range", "bytes 0-2/6")],
                body: b"abc",
            },
            MockResponse {
                status: 206,
                headers: vec![("Content-Range", "bytes 3-5/6")],
                body: b"def",
            },
        ]);
        let mut client = OverleafHttpClient::new(&url).unwrap();
        client.set_identity(identity());
        assert_eq!(client.download("paper.pdf").await.unwrap(), b"abcdef");
        assert!(
            !requests
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("range:")
        );
        assert!(
            requests
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("range: bytes=3-")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn classifies_login_redirect_as_expired_session() {
        let (url, _requests, server) = mock_server(vec![MockResponse {
            status: 302,
            headers: vec![("Location", "/login")],
            body: b"",
        }]);
        let mut client = OverleafHttpClient::new(&url).unwrap();
        client.set_identity(identity());
        let error = client
            .request(Method::GET, "project/x", None, false, REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            HttpError::SessionExpired { status: 302, .. }
        ));
        server.join().unwrap();
    }
}
