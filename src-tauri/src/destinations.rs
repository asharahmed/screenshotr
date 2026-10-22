//! Where screenshots get uploaded. Each destination kind knows how to upload one file and how to
//! check its settings; everything else in the app only sees `upload` and `test`.

use anyhow::{anyhow, Context, Result};
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_credential_types::Credentials;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_types::region::Region;
use serde::{Deserialize, Serialize};
use std::{
  collections::BTreeMap,
  env, fs,
  io::Write,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  time::Duration,
};
use url::Url;

const USER_AGENT: &str = concat!("Screenshotr/", env!("CARGO_PKG_VERSION"));
const CHECK_FILE: &str = ".screenshotr-check";
/// A 1×1 transparent PNG, used where a test has to send a real image.
const TEST_PNG: &[u8] = &[
  0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
  0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49,
  0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00,
  0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Field names that hold credentials. They are kept in the keychain, never in config.json.
pub const SECRET_FIELDS: &[&str] = &[
  "access_key_id",
  "secret_access_key",
  "password",
  "userhash",
  "sas_url",
  "Headers",
  "Arguments",
  "Parameters",
  "Data",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Destination {
  #[serde(default)]
  pub id: String,
  #[serde(default)]
  pub name: String,
  #[serde(flatten)]
  pub kind: Kind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
  S3(S3),
  CustomHttp(CustomUploader),
  Sftp(Sftp),
  Ftp(Ftp),
  Webdav(WebDav),
  Nextcloud(Nextcloud),
  Imgur(Imgur),
  Catbox(Catbox),
  Litterbox(Litterbox),
  AzureBlob(AzureBlob),
  Folder(Folder),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct S3 {
  /// aws, r2, b2, spaces, wasabi, minio or other; only used to fill in defaults.
  pub provider: String,
  pub endpoint: String,
  pub region: String,
  pub bucket: String,
  pub access_key_id: String,
  pub secret_access_key: String,
  pub public_base_url: String,
  pub prefix: String,
  pub path_style: bool,
  pub public_read: bool,
}

/// A ShareX custom uploader (.sxcu). Field names match the file format so imports round-trip.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomUploader {
  #[serde(rename = "RequestMethod", alias = "RequestType")]
  pub request_method: String,
  #[serde(rename = "RequestURL")]
  pub request_url: String,
  #[serde(rename = "Parameters")]
  pub parameters: BTreeMap<String, String>,
  #[serde(rename = "Headers")]
  pub headers: BTreeMap<String, String>,
  #[serde(rename = "Body")]
  pub body: String,
  #[serde(rename = "Arguments")]
  pub arguments: BTreeMap<String, String>,
  #[serde(rename = "FileFormName")]
  pub file_form_name: String,
  #[serde(rename = "Data")]
  pub data: String,
  #[serde(rename = "URL")]
  pub url: String,
  #[serde(rename = "ThumbnailURL")]
  pub thumbnail_url: String,
  #[serde(rename = "DeletionURL")]
  pub deletion_url: String,
  #[serde(rename = "ErrorMessage")]
  pub error_message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sftp {
  /// A host name or an alias from ~/.ssh/config.
  pub host: String,
  pub port: u16,
  pub username: String,
  pub remote_dir: String,
  pub public_base_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ftp {
  pub host: String,
  pub port: u16,
  pub tls: bool,
  pub username: String,
  pub password: String,
  pub remote_dir: String,
  pub public_base_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebDav {
  /// The folder to upload into, e.g. https://dav.example.com/screenshots/
  pub url: String,
  pub username: String,
  pub password: String,
  pub public_base_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Nextcloud {
  pub server: String,
  pub username: String,
  /// An app password from Settings > Security in Nextcloud.
  pub password: String,
  pub folder: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Imgur {
  pub client_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Catbox {
  pub userhash: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Litterbox {
  /// 1h, 12h, 24h or 72h.
  pub time: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AzureBlob {
  /// A container SAS URL with create and write permissions.
  pub sas_url: String,
  pub public_base_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Folder {
  pub path: String,
  pub public_base_url: String,
}

pub struct Uploaded {
  pub url: String,
  pub deletion_url: Option<String>,
  /// The object key, for destinations Screenshotr can delete from directly.
  pub remote_key: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Labels

impl Destination {
  /// Where links point, for status lines: "i.aahmed.ca", "catbox.moe".
  pub fn host_label(&self) -> String {
    let from_url = |url: &str| Url::parse(url.trim()).ok().and_then(|url| url.host_str().map(str::to_string));
    let label = match &self.kind {
      Kind::S3(s3) => from_url(&s3.public_base_url).or_else(|| from_url(&s3_public_base(s3))),
      Kind::CustomHttp(custom) => from_url(&custom.request_url),
      Kind::Sftp(sftp) => from_url(&sftp.public_base_url).or(Some(sftp.host.clone())),
      Kind::Ftp(ftp) => from_url(&ftp.public_base_url).or(Some(ftp.host.clone())),
      Kind::Webdav(dav) => from_url(&dav.public_base_url).or_else(|| from_url(&dav.url)),
      Kind::Nextcloud(cloud) => from_url(&cloud.server),
      Kind::Imgur(_) => Some("imgur.com".to_string()),
      Kind::Catbox(_) => Some("catbox.moe".to_string()),
      Kind::Litterbox(_) => Some("litterbox.catbox.moe".to_string()),
      Kind::AzureBlob(azure) => from_url(&azure.public_base_url).or_else(|| from_url(&azure.sas_url)),
      Kind::Folder(folder) => from_url(&folder.public_base_url),
    };
    label.filter(|label| !label.is_empty()).unwrap_or_else(|| self.name.clone())
  }
}

// ---------------------------------------------------------------------------------------------
// Upload

/// Uploads one file of any type; `mime` is its content type (image/png for captures).
pub async fn upload(destination: &Destination, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  match &destination.kind {
    Kind::S3(s3) => upload_s3(s3, bytes, file_name, mime).await,
    Kind::CustomHttp(custom) => upload_custom(custom, bytes, file_name, mime).await,
    Kind::Sftp(sftp) => upload_sftp(sftp, bytes, file_name, mime),
    Kind::Ftp(ftp) => upload_ftp(ftp, bytes, file_name, mime),
    Kind::Webdav(dav) => upload_webdav(dav, bytes, file_name, mime).await,
    Kind::Nextcloud(cloud) => upload_nextcloud(cloud, bytes, file_name, mime).await,
    Kind::Imgur(imgur) => upload_imgur(imgur, bytes, file_name, mime).await,
    Kind::Catbox(catbox) => upload_catbox(catbox, bytes, file_name, mime).await,
    Kind::Litterbox(litterbox) => upload_litterbox(litterbox, bytes, file_name, mime).await,
    Kind::AzureBlob(azure) => upload_azure(azure, bytes, file_name, mime).await,
    Kind::Folder(folder) => upload_folder(folder, bytes, file_name, mime),
  }
}

/// Whether this destination can take files that aren't images.
pub fn accepts_any_file(destination: &Destination) -> bool {
  !matches!(destination.kind, Kind::Imgur(_))
}

/// Removes an upload from the server, where the destination supports it. Returns false when it
/// can't be done from here (the caller can still forget it locally).
pub async fn delete(destination: &Destination, remote_key: Option<&str>, deletion_url: Option<&str>) -> Result<bool> {
  match &destination.kind {
    Kind::S3(s3) => {
      let Some(key) = remote_key else { return Ok(false) };
      let (client, bucket) = s3_client(s3)?;
      client.delete_object().bucket(&bucket).key(key).send().await.map_err(|error| anyhow!(describe_s3_error(&error)))?;
      Ok(true)
    }
    Kind::Imgur(imgur) => {
      let Some(url) = deletion_url else { return Ok(false) };
      let response = http()?
        .delete(url)
        .header("Authorization", format!("Client-ID {}", imgur.client_id.trim()))
        .send()
        .await
        .context("Couldn't reach Imgur")?;
      if !response.status().is_success() {
        return Err(error_from_response(response, "Imgur didn't delete the image").await);
      }
      Ok(true)
    }
    _ => Ok(false),
  }
}

/// Checks the settings work. Returns a sentence to show next to the Test button.
pub async fn test(destination: &Destination) -> Result<String> {
  match &destination.kind {
    Kind::S3(s3) => {
      let (client, bucket) = s3_client(s3)?;
      let key = format!("{}{CHECK_FILE}", s3_prefix(s3));
      client
        .put_object()
        .bucket(&bucket)
        .key(&key)
        .body(ByteStream::from_static(b"ok"))
        .send()
        .await
        .map_err(|error| anyhow!(describe_s3_error(&error)))?;
      // Not every key may delete; the check file is tiny and harmless if it stays.
      let _ = client.delete_object().bucket(&bucket).key(&key).send().await;
      Ok(format!("Connected. Screenshotr can upload to {bucket}."))
    }
    Kind::Imgur(imgur) => {
      let response = http()?
        .get("https://api.imgur.com/3/credits")
        .header("Authorization", format!("Client-ID {}", imgur.client_id.trim()))
        .send()
        .await
        .context("Couldn't reach Imgur")?;
      if !response.status().is_success() {
        return Err(anyhow!("Imgur didn't accept that client ID"));
      }
      Ok("Connected. Imgur accepted the client ID.".to_string())
    }
    Kind::Catbox(_) | Kind::Litterbox(_) => Ok("Nothing to set up. Uploads are public.".to_string()),
    Kind::Folder(folder) => {
      let dir = expand_home(&folder.path);
      let check = dir.join(CHECK_FILE);
      fs::write(&check, b"ok").with_context(|| format!("Can't write to {}", dir.display()))?;
      let _ = fs::remove_file(check);
      Ok("Screenshotr can save to that folder.".to_string())
    }
    // These have no read-only check, so send a 1-pixel image and report the link it produced.
    _ => {
      let uploaded = upload(destination, TEST_PNG, "screenshotr-test.png", "image/png").await?;
      Ok(format!("Connected. Test image at {}", uploaded.url))
    }
  }
}

fn http() -> Result<reqwest::Client> {
  reqwest::Client::builder()
    .user_agent(USER_AGENT)
    .timeout(Duration::from_secs(60))
    .build()
    .context("failed to start HTTP client")
}

fn join_url(base: &str, name: &str) -> String {
  format!("{}/{}", base.trim().trim_end_matches('/'), name)
}

fn require(value: &str, message: &str) -> Result<()> {
  if value.trim().is_empty() {
    return Err(anyhow!(message.to_string()));
  }
  Ok(())
}

fn expand_home(path: &str) -> PathBuf {
  match path.trim().strip_prefix("~/") {
    Some(rest) => env::var("HOME").map(|home| PathBuf::from(home).join(rest)).unwrap_or_else(|_| PathBuf::from(path)),
    None => PathBuf::from(path.trim()),
  }
}

/// Writes the screenshot to a temporary file for the command-line transfers.
fn temp_upload_file(bytes: &[u8], file_name: &str) -> Result<(tempfile::TempDir, PathBuf)> {
  let dir = tempfile::tempdir().context("failed to create a temporary folder")?;
  let path = dir.path().join(file_name);
  fs::write(&path, bytes).context("failed to write the screenshot")?;
  Ok((dir, path))
}

async fn error_from_response(response: reqwest::Response, fallback: &str) -> anyhow::Error {
  let status = response.status();
  let body = response.text().await.unwrap_or_default();
  let excerpt: String = body.trim().chars().take(160).collect();
  match status.as_u16() {
    401 | 403 => anyhow!("{fallback}: the server refused the credentials ({status})"),
    404 => anyhow!("{fallback}: the address wasn't found (404)"),
    _ if excerpt.is_empty() => anyhow!("{fallback} ({status})"),
    _ => anyhow!("{fallback} ({status}): {excerpt}"),
  }
}

// ---- S3-compatible ----------------------------------------------------------------------------

fn s3_prefix(s3: &S3) -> String {
  let prefix = s3.prefix.trim().trim_matches('/');
  if prefix.is_empty() {
    String::new()
  } else {
    format!("{prefix}/")
  }
}

fn s3_region(s3: &S3) -> String {
  match (s3.region.trim(), s3.provider.as_str()) {
    ("", "r2") => "auto".to_string(),
    ("", _) => "us-east-1".to_string(),
    (region, _) => region.to_string(),
  }
}

/// The public address of the bucket when no base URL is given, following each provider's scheme.
pub fn s3_public_base(s3: &S3) -> String {
  let bucket = s3.bucket.trim();
  let region = s3_region(s3);
  match s3.provider.as_str() {
    "aws" => format!("https://{bucket}.s3.{region}.amazonaws.com"),
    "spaces" => format!("https://{bucket}.{region}.digitaloceanspaces.com"),
    "wasabi" => format!("https://s3.{region}.wasabisys.com/{bucket}"),
    // R2's S3 endpoint isn't public; it needs an r2.dev or custom domain.
    "r2" => String::new(),
    _ if !s3.endpoint.trim().is_empty() => join_url(&s3_endpoint(s3), bucket),
    _ => String::new(),
  }
}

fn s3_endpoint(s3: &S3) -> String {
  let endpoint = s3.endpoint.trim();
  if endpoint.is_empty() && s3.provider == "aws" {
    return format!("https://s3.{}.amazonaws.com", s3_region(s3));
  }
  match Url::parse(endpoint) {
    Ok(mut url) => {
      url.set_path("");
      url.to_string().trim_end_matches('/').to_string()
    }
    Err(_) => endpoint.to_string(),
  }
}

fn s3_client(s3: &S3) -> Result<(aws_sdk_s3::Client, String)> {
  require(&s3.access_key_id, "Add the access key ID")?;
  require(&s3.secret_access_key, "Add the secret access key")?;
  // Older settings put the bucket in the endpoint path.
  let mut bucket = s3.bucket.trim().to_string();
  if bucket.is_empty() {
    if let Ok(url) = Url::parse(s3.endpoint.trim()) {
      bucket = url.path().trim_matches('/').to_string();
    }
  }
  require(&bucket, "Add the bucket name")?;
  let endpoint = s3_endpoint(s3);
  if Url::parse(&endpoint).is_err() {
    return Err(anyhow!("The endpoint isn't a valid URL"));
  }

  let creds = Credentials::new(s3.access_key_id.trim(), s3.secret_access_key.trim(), None, None, "Screenshotr");
  let config = aws_sdk_s3::config::Builder::new()
    .region(Region::new(s3_region(s3)))
    .credentials_provider(SharedCredentialsProvider::new(creds))
    .endpoint_url(endpoint)
    .force_path_style(s3.path_style || s3.provider == "minio")
    .build();
  Ok((aws_sdk_s3::Client::from_conf(config), bucket))
}

/// Turns SDK errors into one short sentence a person can act on.
fn describe_s3_error<E: ProvideErrorMetadata, R>(error: &SdkError<E, R>) -> String {
  match error {
    SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) => "Couldn't reach the storage endpoint".to_string(),
    SdkError::ServiceError(service) => {
      let inner = service.err();
      match inner.code() {
        Some("InvalidAccessKeyId") | Some("SignatureDoesNotMatch") | Some("Unauthorized") => {
          "The access key ID or secret wasn't accepted".to_string()
        }
        Some("AccessDenied") => "These keys can't write to the bucket".to_string(),
        Some("NoSuchBucket") => "That bucket doesn't exist".to_string(),
        Some("AccessControlListNotSupported") => "This bucket doesn't allow public-read; turn that option off".to_string(),
        _ => inner
          .message()
          .or(inner.code())
          .map(str::to_string)
          .unwrap_or_else(|| "The storage service rejected the upload".to_string()),
      }
    }
    _ => "The storage service rejected the upload".to_string(),
  }
}

async fn upload_s3(s3: &S3, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  let (client, bucket) = s3_client(s3)?;
  let key = format!("{}{file_name}", s3_prefix(s3));
  let mut request = client
    .put_object()
    .bucket(&bucket)
    .key(&key)
    .content_type(mime)
    .body(ByteStream::from(bytes.to_vec()));
  if s3.public_read {
    request = request.acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead);
  }
  request.send().await.map_err(|error| anyhow!(describe_s3_error(&error)))?;

  let base = if s3.public_base_url.trim().is_empty() { s3_public_base(s3) } else { s3.public_base_url.clone() };
  if base.is_empty() {
    return Err(anyhow!("Uploaded, but there's no public URL set for links"));
  }
  Ok(Uploaded { url: join_url(&base, &key), deletion_url: None, remote_key: Some(key) })
}

// ---- ShareX custom uploader ---------------------------------------------------------------------

/// Reads a .sxcu file. The display name comes from its Name field.
pub fn import_sxcu(contents: &str) -> Result<Destination> {
  let value: serde_json::Value = serde_json::from_str(contents.trim_start_matches('\u{feff}'))
    .context("This file isn't a ShareX custom uploader")?;
  let name = value.get("Name").and_then(|name| name.as_str()).unwrap_or("Custom uploader").to_string();
  let kind = value.get("DestinationType").and_then(|kind| kind.as_str()).unwrap_or("ImageUploader");
  if !kind.contains("ImageUploader") && !kind.contains("FileUploader") {
    return Err(anyhow!("This uploader is for {kind}, not images"));
  }
  let mut uploader: CustomUploader = serde_json::from_value(value).context("This uploader has settings Screenshotr can't read")?;
  if uploader.request_url.trim().is_empty() {
    return Err(anyhow!("This uploader has no request URL"));
  }
  if uploader.body.is_empty() {
    // Version 12 and older files have no Body; a file field means multipart.
    uploader.body = if uploader.file_form_name.is_empty() { "Binary" } else { "MultipartFormData" }.to_string();
  }
  Ok(Destination { id: String::new(), name, kind: Kind::CustomHttp(uploader) })
}

struct TemplateContext<'a> {
  file_name: &'a str,
  response: &'a str,
  response_url: &'a str,
  headers: &'a reqwest::header::HeaderMap,
}

/// Fills ShareX placeholders: {json:path}, {regex:pattern|group}, {header:name}, {response},
/// {responseurl}, {filename}, {random:a|b}. Older $json:path$ syntax is accepted too.
fn fill_template(template: &str, context: &TemplateContext) -> Result<String> {
  let legacy = regex::Regex::new(r"\$(json|xml|regex|header|response|filename|random)(:[^$]*)?\$").unwrap();
  let template = legacy.replace_all(template, |caps: &regex::Captures| {
    format!("{{{}{}}}", &caps[1], caps.get(2).map(|m| m.as_str()).unwrap_or(""))
  });

  let mut out = String::new();
  let mut rest: &str = &template;
  while let Some(start) = rest.find('{') {
    out.push_str(&rest[..start]);
    let Some(len) = rest[start..].find('}') else {
      out.push_str(&rest[start..]);
      rest = "";
      break;
    };
    let token = &rest[start + 1..start + len];
    let (name, arg) = token.split_once(':').unwrap_or((token, ""));
    let value = match name {
      "response" => context.response.to_string(),
      "responseurl" => context.response_url.to_string(),
      "filename" => context.file_name.to_string(),
      "random" => {
        let options: Vec<&str> = arg.split('|').collect();
        options[rand::random::<usize>() % options.len()].to_string()
      }
      "header" => context
        .headers
        .get(arg)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string(),
      "json" => json_path(context.response, arg)?,
      "regex" => {
        let (pattern, group) = arg.rsplit_once('|').unwrap_or((arg, "1"));
        let re = regex::Regex::new(pattern).context("The uploader's regex is invalid")?;
        let caps = re.captures(context.response).ok_or_else(|| anyhow!("The response didn't contain a link"))?;
        let found = match group.parse::<usize>() {
          Ok(index) => caps.get(index),
          Err(_) => caps.name(group),
        };
        found.map(|m| m.as_str().to_string()).unwrap_or_default()
      }
      "xml" => return Err(anyhow!("Uploaders that answer in XML aren't supported yet")),
      // Not a placeholder, e.g. JSON braces in a body template.
      _ => format!("{{{token}}}"),
    };
    out.push_str(&value);
    rest = &rest[start + len + 1..];
  }
  out.push_str(rest);
  Ok(out)
}

/// Follows a path like `data.link`, `files[0].url` or `$.data.link` into a JSON response.
fn json_path(response: &str, path: &str) -> Result<String> {
  let value: serde_json::Value =
    serde_json::from_str(response).map_err(|_| anyhow!("The server didn't answer with JSON"))?;
  let mut current = &value;
  let path = path.trim().trim_start_matches('$').trim_start_matches('.');
  for part in path.split('.').filter(|part| !part.is_empty()) {
    let (key, indexes) = match part.find('[') {
      Some(at) => (&part[..at], &part[at..]),
      None => (part, ""),
    };
    if !key.is_empty() {
      current = current.get(key).ok_or_else(|| anyhow!("The response has no {path}"))?;
    }
    for index in indexes.split(['[', ']']).filter(|index| !index.is_empty()) {
      let index: usize = index.parse().map_err(|_| anyhow!("Can't read {path} from the response"))?;
      current = current.get(index).ok_or_else(|| anyhow!("The response has no {path}"))?;
    }
  }
  Ok(match current {
    serde_json::Value::String(text) => text.clone(),
    other => other.to_string(),
  })
}

async fn upload_custom(custom: &CustomUploader, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  let empty_headers = reqwest::header::HeaderMap::new();
  let request_context =
    TemplateContext { file_name, response: "", response_url: "", headers: &empty_headers };
  let fill = |text: &str| fill_template(text, &request_context);

  let mut url = Url::parse(&fill(&custom.request_url)?).context("The uploader's request URL is invalid")?;
  for (key, value) in &custom.parameters {
    url.query_pairs_mut().append_pair(key, &fill(value)?);
  }
  let method = match custom.request_method.to_ascii_uppercase().as_str() {
    "" | "POST" => reqwest::Method::POST,
    "PUT" => reqwest::Method::PUT,
    "PATCH" => reqwest::Method::PATCH,
    "GET" => reqwest::Method::GET,
    other => return Err(anyhow!("The uploader uses an unsupported method ({other})")),
  };
  let mut request = http()?.request(method, url);
  for (key, value) in &custom.headers {
    request = request.header(key, fill(value)?);
  }
  request = match custom.body.as_str() {
    "MultipartFormData" => {
      let mut form = reqwest::multipart::Form::new();
      for (key, value) in &custom.arguments {
        form = form.text(key.clone(), fill(value)?);
      }
      let field = if custom.file_form_name.is_empty() { "file".to_string() } else { custom.file_form_name.clone() };
      let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(file_name.to_string()).mime_str(mime)?;
      request.multipart(form.part(field, part))
    }
    "Binary" => request.header("Content-Type", mime).body(bytes.to_vec()),
    "FormURLEncoded" => {
      let mut fields = Vec::new();
      for (key, value) in &custom.arguments {
        fields.push((key.clone(), fill(value)?));
      }
      request.form(&fields)
    }
    "JSON" => request.header("Content-Type", "application/json").body(fill(&custom.data)?),
    "None" => request,
    other => return Err(anyhow!("The uploader uses an unsupported body type ({other})")),
  };

  let response = request.send().await.context("Couldn't reach the upload server")?;
  let status = response.status();
  let response_url = response.url().to_string();
  let headers = response.headers().clone();
  let body = response.text().await.unwrap_or_default();
  let context = TemplateContext { file_name, response: &body, response_url: &response_url, headers: &headers };
  if !status.is_success() {
    let message = if custom.error_message.is_empty() {
      body.trim().chars().take(160).collect()
    } else {
      fill_template(&custom.error_message, &context).unwrap_or_default()
    };
    return Err(anyhow!("The server refused the upload ({status}){}", if message.is_empty() { String::new() } else { format!(": {message}") }));
  }

  let url = if custom.url.trim().is_empty() { body.trim().to_string() } else { fill_template(&custom.url, &context)? };
  if !url.starts_with("http") {
    return Err(anyhow!("The server's answer didn't include a link"));
  }
  let deletion_url = if custom.deletion_url.trim().is_empty() {
    None
  } else {
    fill_template(&custom.deletion_url, &context).ok().filter(|url| !url.is_empty())
  };
  Ok(Uploaded { url, deletion_url, remote_key: None })
}

// ---- SFTP (system OpenSSH) ----------------------------------------------------------------------

/// Uses the system's `sftp`, so the person's SSH keys, agent and ~/.ssh/config all apply.
fn upload_sftp(sftp: &Sftp, bytes: &[u8], file_name: &str, _mime: &str) -> Result<Uploaded> {
  require(&sftp.host, "Add the server's host name")?;
  let (_dir, local) = temp_upload_file(bytes, file_name)?;
  let remote_dir = sftp.remote_dir.trim().trim_end_matches('/');
  let remote = if remote_dir.is_empty() { file_name.to_string() } else { format!("{remote_dir}/{file_name}") };
  let script = format!("put \"{}\" \"{}\"\n", local.display(), remote.replace('"', "\\\""));
  run_sftp(sftp, &script)?;
  if sftp.public_base_url.trim().is_empty() {
    return Err(anyhow!("Uploaded, but there's no public URL set for links"));
  }
  Ok(Uploaded { url: join_url(&sftp.public_base_url, file_name), deletion_url: None, remote_key: None })
}

fn run_sftp(sftp: &Sftp, script: &str) -> Result<()> {
  let target = if sftp.username.trim().is_empty() {
    sftp.host.trim().to_string()
  } else {
    format!("{}@{}", sftp.username.trim(), sftp.host.trim())
  };
  let mut command = Command::new("sftp");
  command.args(["-q", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-b", "-"]);
  if sftp.port != 0 {
    command.args(["-P", &sftp.port.to_string()]);
  }
  let mut child = command
    .arg(target)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .context("Couldn't run sftp")?;
  child.stdin.take().ok_or_else(|| anyhow!("sftp has no input"))?.write_all(script.as_bytes())?;
  let output = child.wait_with_output()?;
  if output.status.success() {
    return Ok(());
  }
  let stderr = String::from_utf8_lossy(&output.stderr);
  let reason = if stderr.contains("Permission denied") {
    "The server refused the SSH key. Add your key to ssh-agent or ~/.ssh/config".to_string()
  } else if stderr.contains("Could not resolve") || stderr.contains("timed out") {
    "Couldn't reach the server".to_string()
  } else if stderr.contains("No such file") {
    "The remote folder doesn't exist".to_string()
  } else {
    stderr.lines().last().unwrap_or("The upload failed").trim().to_string()
  };
  Err(anyhow!(reason))
}

// ---- FTP / FTPS (system curl) -------------------------------------------------------------------

fn upload_ftp(ftp: &Ftp, bytes: &[u8], file_name: &str, _mime: &str) -> Result<Uploaded> {
  require(&ftp.host, "Add the server's host name")?;
  let (_dir, local) = temp_upload_file(bytes, file_name)?;
  run_curl_ftp(ftp, &local, file_name, None)?;
  if ftp.public_base_url.trim().is_empty() {
    return Err(anyhow!("Uploaded, but there's no public URL set for links"));
  }
  Ok(Uploaded { url: join_url(&ftp.public_base_url, file_name), deletion_url: None, remote_key: None })
}

/// Credentials go to curl on stdin, so they never appear in the process list.
fn run_curl_ftp(ftp: &Ftp, local: &Path, remote_name: &str, delete_after: Option<&str>) -> Result<()> {
  let host = ftp.host.trim().trim_start_matches("ftp://").trim_start_matches("ftps://").trim_end_matches('/');
  let port = if ftp.port == 0 { String::new() } else { format!(":{}", ftp.port) };
  let dir = ftp.remote_dir.trim().trim_matches('/');
  let dir = if dir.is_empty() { String::new() } else { format!("{dir}/") };
  let url = format!("ftp://{host}{port}/{dir}{remote_name}");
  let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
  let config = format!("user = \"{}:{}\"\n", escape(ftp.username.trim()), escape(&ftp.password));

  let mut command = Command::new("curl");
  command.args(["-sS", "--ftp-create-dirs", "--connect-timeout", "10", "-K", "-", "-T"]).arg(local).arg(&url);
  if ftp.tls {
    command.arg("--ssl-reqd");
  }
  if let Some(name) = delete_after {
    command.args(["-Q", &format!("-DELE {name}")]);
  }
  let mut child = command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("Couldn't run curl")?;
  child.stdin.take().ok_or_else(|| anyhow!("curl has no input"))?.write_all(config.as_bytes())?;
  let output = child.wait_with_output()?;
  if output.status.success() {
    return Ok(());
  }
  let reason = match output.status.code() {
    Some(6) | Some(7) | Some(28) => "Couldn't reach the server".to_string(),
    Some(9) => "The remote folder isn't accessible".to_string(),
    Some(67) => "The server refused the user name or password".to_string(),
    Some(64) => "This server doesn't support FTPS; turn off TLS".to_string(),
    _ => String::from_utf8_lossy(&output.stderr).trim().trim_start_matches("curl: ").to_string(),
  };
  Err(anyhow!(reason))
}

// ---- WebDAV and Nextcloud -----------------------------------------------------------------------

async fn upload_webdav(dav: &WebDav, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  require(&dav.url, "Add the WebDAV folder URL")?;
  dav_put(&dav.url, &dav.username, &dav.password, bytes, file_name, mime).await?;
  let base = if dav.public_base_url.trim().is_empty() { &dav.url } else { &dav.public_base_url };
  Ok(Uploaded { url: join_url(base, file_name), deletion_url: None, remote_key: None })
}

/// PUTs into a WebDAV folder, creating the folder once if the server says it's missing.
async fn dav_put(folder: &str, username: &str, password: &str, bytes: &[u8], file_name: &str, mime: &str) -> Result<()> {
  let client = http()?;
  let target = join_url(folder, file_name);
  for attempt in 0..2 {
    let response = client
      .put(&target)
      .basic_auth(username.trim(), Some(password))
      .header("Content-Type", mime)
      .body(bytes.to_vec())
      .send()
      .await
      .context("Couldn't reach the WebDAV server")?;
    let status = response.status().as_u16();
    if response.status().is_success() {
      return Ok(());
    }
    if attempt == 0 && (status == 404 || status == 409) {
      let mkcol = reqwest::Method::from_bytes(b"MKCOL").unwrap();
      let _ = client.request(mkcol, folder.trim()).basic_auth(username.trim(), Some(password)).send().await;
      continue;
    }
    return Err(error_from_response(response, "The WebDAV server refused the upload").await);
  }
  Err(anyhow!("The WebDAV folder couldn't be created"))
}

async fn upload_nextcloud(cloud: &Nextcloud, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  require(&cloud.server, "Add your Nextcloud address")?;
  require(&cloud.username, "Add your Nextcloud user name")?;
  require(&cloud.password, "Add an app password")?;
  let server = cloud.server.trim().trim_end_matches('/');
  let folder = cloud.folder.trim().trim_matches('/');
  let folder = if folder.is_empty() { "Screenshots" } else { folder };
  let dav_folder = format!("{server}/remote.php/dav/files/{}/{folder}", cloud.username.trim());
  dav_put(&dav_folder, &cloud.username, &cloud.password, bytes, file_name, mime).await?;

  // Share it publicly (shareType 3 = public link, read-only).
  let response = http()?
    .post(format!("{server}/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json"))
    .basic_auth(cloud.username.trim(), Some(&cloud.password))
    .header("OCS-APIRequest", "true")
    .form(&[("path", format!("/{folder}/{file_name}")), ("shareType", "3".to_string()), ("permissions", "1".to_string())])
    .send()
    .await
    .context("Couldn't reach Nextcloud")?;
  if !response.status().is_success() {
    return Err(error_from_response(response, "Nextcloud couldn't create a public link").await);
  }
  let body = response.text().await.unwrap_or_default();
  let link = json_path(&body, "ocs.data.url")?;
  // /preview shows the image itself instead of the share page.
  Ok(Uploaded { url: format!("{}/preview", link.trim_end_matches('/')), deletion_url: None, remote_key: None })
}

// ---- Image hosts --------------------------------------------------------------------------------

async fn upload_imgur(imgur: &Imgur, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  require(&imgur.client_id, "Add an Imgur client ID")?;
  if !mime.starts_with("image/") && !mime.starts_with("video/") {
    return Err(anyhow!("Imgur only takes images and videos"));
  }
  let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(file_name.to_string()).mime_str(mime)?;
  let response = http()?
    .post("https://api.imgur.com/3/image")
    .header("Authorization", format!("Client-ID {}", imgur.client_id.trim()))
    .multipart(reqwest::multipart::Form::new().text("type", "file").part("image", part))
    .send()
    .await
    .context("Couldn't reach Imgur")?;
  if !response.status().is_success() {
    return Err(error_from_response(response, "Imgur refused the upload").await);
  }
  let body = response.text().await.unwrap_or_default();
  let url = json_path(&body, "data.link")?;
  let deletion_url = json_path(&body, "data.deletehash").ok().map(|hash| format!("https://api.imgur.com/3/image/{hash}"));
  Ok(Uploaded { url, deletion_url, remote_key: None })
}

async fn catbox_style_upload(endpoint: &str, fields: Vec<(&str, String)>, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  let mut form = reqwest::multipart::Form::new().text("reqtype", "fileupload");
  for (key, value) in fields {
    form = form.text(key.to_string(), value);
  }
  let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(file_name.to_string()).mime_str(mime)?;
  let response = http()?.post(endpoint).multipart(form.part("fileToUpload", part)).send().await.context("Couldn't reach the host")?;
  if !response.status().is_success() {
    return Err(error_from_response(response, "The host refused the upload").await);
  }
  let url = response.text().await.unwrap_or_default().trim().to_string();
  if !url.starts_with("https://") {
    return Err(anyhow!("The host refused the upload: {}", url.chars().take(120).collect::<String>()));
  }
  Ok(Uploaded { url, deletion_url: None, remote_key: None })
}

async fn upload_catbox(catbox: &Catbox, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  let mut fields = Vec::new();
  if !catbox.userhash.trim().is_empty() {
    fields.push(("userhash", catbox.userhash.trim().to_string()));
  }
  catbox_style_upload("https://catbox.moe/user/api.php", fields, bytes, file_name, mime).await
}

async fn upload_litterbox(litterbox: &Litterbox, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  let time = match litterbox.time.as_str() {
    "1h" | "12h" | "24h" | "72h" => litterbox.time.clone(),
    _ => "24h".to_string(),
  };
  catbox_style_upload("https://litterbox.catbox.moe/resources/internals/api.php", vec![("time", time)], bytes, file_name, mime).await
}

// ---- Azure Blob Storage -------------------------------------------------------------------------

async fn upload_azure(azure: &AzureBlob, bytes: &[u8], file_name: &str, mime: &str) -> Result<Uploaded> {
  require(&azure.sas_url, "Paste a container SAS URL")?;
  let sas = Url::parse(azure.sas_url.trim()).context("The SAS URL isn't valid")?;
  let query = sas.query().ok_or_else(|| anyhow!("The SAS URL is missing its token (the part after ?)"))?.to_string();
  let mut container = sas.clone();
  container.set_query(None);
  let container = container.to_string();
  let response = http()?
    .put(format!("{}?{query}", join_url(&container, file_name)))
    .header("x-ms-blob-type", "BlockBlob")
    .header("Content-Type", mime)
    .body(bytes.to_vec())
    .send()
    .await
    .context("Couldn't reach Azure")?;
  if !response.status().is_success() {
    return Err(error_from_response(response, "Azure refused the upload").await);
  }
  let base = if azure.public_base_url.trim().is_empty() { container } else { azure.public_base_url.clone() };
  Ok(Uploaded { url: join_url(&base, file_name), deletion_url: None, remote_key: None })
}

// ---- Folder -------------------------------------------------------------------------------------

/// For folders a web server or sync service already publishes.
fn upload_folder(folder: &Folder, bytes: &[u8], file_name: &str, _mime: &str) -> Result<Uploaded> {
  require(&folder.path, "Choose a folder")?;
  require(&folder.public_base_url, "Add the public URL for this folder")?;
  let dir = expand_home(&folder.path);
  fs::create_dir_all(&dir).with_context(|| format!("Can't create {}", dir.display()))?;
  fs::write(dir.join(file_name), bytes).with_context(|| format!("Can't write to {}", dir.display()))?;
  Ok(Uploaded { url: join_url(&folder.public_base_url, file_name), deletion_url: None, remote_key: None })
}

// ---------------------------------------------------------------------------------------------
// Cloudflare R2 quick connect

#[derive(Deserialize)]
struct CloudflareResponse<T> {
  success: bool,
  #[serde(default)]
  errors: Vec<CloudflareError>,
  result: Option<T>,
}

#[derive(Deserialize)]
struct CloudflareError {
  code: i64,
  message: String,
}

async fn cloudflare<T: serde::de::DeserializeOwned>(
  client: &reqwest::Client,
  method: reqwest::Method,
  path: &str,
  token: &str,
  body: Option<serde_json::Value>,
) -> Result<std::result::Result<T, Vec<CloudflareError>>> {
  let mut request = client.request(method, format!("https://api.cloudflare.com/client/v4{path}")).bearer_auth(token);
  if let Some(body) = body {
    request = request.json(&body);
  }
  let response = request.send().await.context("Couldn't reach Cloudflare")?;
  let parsed: CloudflareResponse<T> = response.json().await.context("Cloudflare sent an unexpected answer")?;
  Ok(match (parsed.success, parsed.result) {
    (true, Some(result)) => Ok(result),
    _ => Err(parsed.errors),
  })
}

/// Sets up R2 from a single API token: finds the account, creates the bucket if needed, turns on its
/// public r2.dev address, and derives S3 credentials from the token (id + SHA-256 of the value).
pub async fn connect_r2(token: &str, bucket: &str) -> Result<Destination> {
  use sha2::{Digest, Sha256};

  let token = token.trim();
  require(token, "Paste the API token")?;
  let bucket = if bucket.trim().is_empty() { "screenshots" } else { bucket.trim() };
  let client = http()?;

  #[derive(Deserialize)]
  struct Account {
    id: String,
  }
  #[derive(Deserialize)]
  struct Token {
    id: String,
  }
  #[derive(Deserialize)]
  struct Managed {
    domain: String,
  }

  let accounts: Vec<Account> = cloudflare(&client, reqwest::Method::GET, "/accounts", token, None)
    .await?
    .map_err(|_| anyhow!("Cloudflare didn't accept that token"))?;
  let account = accounts.first().ok_or_else(|| anyhow!("That token can't see any Cloudflare account"))?;

  // User tokens verify at /user, account-owned tokens under the account.
  let token_id = match cloudflare::<Token>(&client, reqwest::Method::GET, "/user/tokens/verify", token, None).await? {
    Ok(found) => found.id,
    Err(_) => cloudflare::<Token>(&client, reqwest::Method::GET, &format!("/accounts/{}/tokens/verify", account.id), token, None)
      .await?
      .map_err(|_| anyhow!("Cloudflare didn't accept that token"))?
      .id,
  };

  let created = cloudflare::<serde_json::Value>(
    &client,
    reqwest::Method::POST,
    &format!("/accounts/{}/r2/buckets", account.id),
    token,
    Some(serde_json::json!({ "name": bucket })),
  )
  .await?;
  if let Err(errors) = created {
    // 10004: the bucket already exists, which is fine.
    if !errors.iter().any(|error| error.code == 10004 || error.message.contains("already exists")) {
      let reason = errors.first().map(|error| error.message.clone()).unwrap_or_default();
      return Err(anyhow!("Couldn't create the bucket. Check the token has R2 Edit permission. {reason}"));
    }
  }

  let managed: Managed = cloudflare(
    &client,
    reqwest::Method::PUT,
    &format!("/accounts/{}/r2/buckets/{bucket}/domains/managed", account.id),
    token,
    Some(serde_json::json!({ "enabled": true })),
  )
  .await?
  .map_err(|errors| {
    anyhow!("Couldn't turn on public links. {}", errors.first().map(|error| error.message.as_str()).unwrap_or(""))
  })?;

  let secret = Sha256::digest(token.as_bytes());
  Ok(Destination {
    id: String::new(),
    name: "Cloudflare R2".to_string(),
    kind: Kind::S3(S3 {
      provider: "r2".to_string(),
      endpoint: format!("https://{}.r2.cloudflarestorage.com", account.id),
      region: "auto".to_string(),
      bucket: bucket.to_string(),
      access_key_id: token_id,
      secret_access_key: hex::encode(secret),
      public_base_url: format!("https://{}", managed.domain),
      ..S3::default()
    }),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn context<'a>(response: &'a str, headers: &'a reqwest::header::HeaderMap) -> TemplateContext<'a> {
    TemplateContext { file_name: "shot.png", response, response_url: "https://up.example/api", headers }
  }

  #[test]
  fn fills_json_paths_in_both_syntaxes() {
    let headers = reqwest::header::HeaderMap::new();
    let response = r#"{"data":{"link":"https://i.example/a.png"},"files":[{"url":"https://f.example/b.png"}]}"#;
    let ctx = context(response, &headers);
    assert_eq!(fill_template("{json:data.link}", &ctx).unwrap(), "https://i.example/a.png");
    assert_eq!(fill_template("$json:files[0].url$", &ctx).unwrap(), "https://f.example/b.png");
    assert_eq!(fill_template("{json:$.data.link}?raw", &ctx).unwrap(), "https://i.example/a.png?raw");
  }

  #[test]
  fn fills_regex_response_and_filename() {
    let headers = reqwest::header::HeaderMap::new();
    let ctx = context("Uploaded: https://x.example/abc.png done", &headers);
    assert_eq!(fill_template(r"{regex:(https://\S+)|1}", &ctx).unwrap(), "https://x.example/abc.png");
    assert_eq!(fill_template("https://cdn.example/{filename}", &ctx).unwrap(), "https://cdn.example/shot.png");
    assert_eq!(fill_template("{response}", &ctx).unwrap(), "Uploaded: https://x.example/abc.png done");
  }

  #[test]
  fn imports_sxcu_files() {
    let sxcu = r#"{
      "Version": "15.0.0",
      "Name": "Zipline",
      "DestinationType": "ImageUploader, FileUploader",
      "RequestMethod": "POST",
      "RequestURL": "https://zip.example/api/upload",
      "Headers": { "Authorization": "token" },
      "Body": "MultipartFormData",
      "FileFormName": "file",
      "URL": "{json:files[0]}"
    }"#;
    let destination = import_sxcu(sxcu).unwrap();
    assert_eq!(destination.name, "Zipline");
    let Kind::CustomHttp(custom) = destination.kind else { panic!("wrong kind") };
    assert_eq!(custom.request_url, "https://zip.example/api/upload");
    assert_eq!(custom.headers.get("Authorization").map(String::as_str), Some("token"));
    assert_eq!(custom.url, "{json:files[0]}");
  }

  #[test]
  fn old_sxcu_without_body_uses_multipart() {
    let sxcu = r#"{"Name":"Old","RequestType":"POST","RequestURL":"https://old.example/up","FileFormName":"img","URL":"$json:url$"}"#;
    let Kind::CustomHttp(custom) = import_sxcu(sxcu).unwrap().kind else { panic!("wrong kind") };
    assert_eq!(custom.body, "MultipartFormData");
    assert_eq!(custom.request_method, "POST");
  }

  #[test]
  fn rejects_non_image_uploaders() {
    let sxcu = r#"{"Name":"Shortener","DestinationType":"URLShortener","RequestURL":"https://s.example"}"#;
    assert!(import_sxcu(sxcu).is_err());
  }

  #[test]
  fn derives_public_urls_per_provider() {
    let aws = S3 { provider: "aws".into(), bucket: "shots".into(), region: "eu-west-1".into(), ..S3::default() };
    assert_eq!(s3_public_base(&aws), "https://shots.s3.eu-west-1.amazonaws.com");
    let minio = S3 { provider: "minio".into(), bucket: "shots".into(), endpoint: "https://minio.example:9000/".into(), ..S3::default() };
    assert_eq!(s3_public_base(&minio), "https://minio.example:9000/shots");
    let r2 = S3 { provider: "r2".into(), bucket: "shots".into(), ..S3::default() };
    assert_eq!(s3_public_base(&r2), "");
  }

  #[test]
  fn destinations_round_trip_through_json() {
    let destination = Destination {
      id: "abc".into(),
      name: "Server".into(),
      kind: Kind::Sftp(Sftp { host: "box".into(), remote_dir: "/var/www/i".into(), ..Sftp::default() }),
    };
    let json = serde_json::to_value(&destination).unwrap();
    assert_eq!(json["kind"], "sftp");
    assert_eq!(json["host"], "box");
    let back: Destination = serde_json::from_value(json).unwrap();
    assert!(matches!(back.kind, Kind::Sftp(_)));
  }
}
