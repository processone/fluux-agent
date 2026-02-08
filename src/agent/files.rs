/// File download and processing for OOB (XEP-0066) attachments.
///
/// Downloads files from HTTP Upload URLs, validates them (size, MIME type),
/// and converts supported types to Anthropic API content blocks for
/// multi-modal LLM processing.
///
/// All I/O in this module uses `tokio::fs` to avoid blocking the async
/// runtime (images/PDFs can be several MB).
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use base64::Engine;
use reqwest::Client;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::llm::{DocumentSource, ImageSource, InputContentBlock};

/// Maximum file size: 25 MB
const MAX_FILE_SIZE: u64 = 25 * 1024 * 1024;

/// Download timeout: 30 seconds
const DOWNLOAD_TIMEOUT_SECS: u64 = 30;

/// File category based on MIME type
#[derive(Debug, Clone, PartialEq)]
pub enum FileCategory {
    /// Image file (jpeg, png, gif, webp) — sent as image content block
    Image,
    /// Document file (pdf) — sent as document content block
    Document,
    /// Unsupported type — stored but not sent to LLM
    Other,
}

/// Result of a successful file download
#[derive(Debug)]
pub struct DownloadedFile {
    /// Absolute path to the downloaded file
    pub path: PathBuf,
    /// Original filename extracted from the URL
    pub filename: String,
    /// Detected MIME type
    pub mime_type: String,
    /// File size in bytes
    pub size: u64,
    /// Category for LLM processing
    pub category: FileCategory,
}

impl DownloadedFile {
    /// Returns a human-readable size string (e.g., "45KB", "1.2MB")
    pub fn human_size(&self) -> String {
        if self.size < 1024 {
            format!("{}B", self.size)
        } else if self.size < 1024 * 1024 {
            format!("{}KB", self.size / 1024)
        } else {
            format!("{:.1}MB", self.size as f64 / (1024.0 * 1024.0))
        }
    }
}

/// File download service with concurrency limiting.
///
/// Downloads files from OOB URLs (XEP-0066 / XEP-0363 HTTP Upload),
/// validates them, and stores them in the per-JID files directory.
pub struct FileDownloader {
    client: Client,
    semaphore: Arc<Semaphore>,
}

impl FileDownloader {
    /// Creates a new downloader with the given concurrency limit.
    ///
    /// When `danger_accept_invalid_certs` is true, the HTTP client will
    /// accept self-signed TLS certificates (useful when the XMPP HTTP
    /// Upload service shares the same self-signed cert as the XMPP server).
    pub fn new(max_concurrent: usize) -> Self {
        Self::with_tls_verify(max_concurrent, true)
    }

    /// Creates a new downloader with explicit TLS verification setting.
    pub fn with_tls_verify(max_concurrent: usize, tls_verify: bool) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
            .connect_timeout(std::time::Duration::from_secs(10))
            .danger_accept_invalid_certs(!tls_verify)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Downloads a file from a URL and saves it to the given directory.
    ///
    /// - Validates the URL scheme (HTTPS required, except localhost for dev)
    /// - Checks Content-Length header against the 25MB limit
    /// - Determines MIME type from Content-Type header and filename extension
    /// - Saves the file with a UUID prefix to prevent collisions
    ///
    /// Returns a `DownloadedFile` with metadata about the download.
    pub async fn download(&self, url: &str, files_dir: &Path) -> Result<DownloadedFile> {
        // Acquire semaphore permit (limits concurrent downloads)
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| anyhow!("Semaphore closed: {e}"))?;

        // Validate URL scheme
        let parsed = url::Url::parse(url).map_err(|e| anyhow!("Invalid URL: {e}"))?;
        let host = parsed.host_str().unwrap_or("");
        let is_local = host == "localhost" || host == "127.0.0.1" || host == "::1";
        if parsed.scheme() != "https" && !is_local {
            return Err(anyhow!(
                "Only HTTPS URLs are allowed (got {}://)",
                parsed.scheme()
            ));
        }

        info!("Downloading file from {url}");

        // Send GET request
        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Download failed: HTTP {}",
                response.status()
            ));
        }

        // Check Content-Length before downloading
        if let Some(content_length) = response.content_length() {
            if content_length > MAX_FILE_SIZE {
                return Err(anyhow!(
                    "File too large: {} bytes (max {})",
                    content_length,
                    MAX_FILE_SIZE
                ));
            }
        }

        // Determine MIME type from Content-Type header
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_default();

        // Extract filename from URL path
        let url_filename = parsed
            .path_segments()
            .and_then(|segs| segs.last())
            .unwrap_or("file")
            .to_string();

        // Download the body (with size enforcement)
        let bytes = response.bytes().await?;
        let size = bytes.len() as u64;

        if size > MAX_FILE_SIZE {
            return Err(anyhow!(
                "File too large: {} bytes (max {})",
                size,
                MAX_FILE_SIZE
            ));
        }

        // Determine final MIME type (Content-Type header, fallback to extension)
        let mime_type = if content_type.is_empty() || content_type == "application/octet-stream" {
            mime_from_extension(&url_filename)
        } else {
            content_type
        };

        let category = categorize_mime(&mime_type);

        // Save to disk with UUID prefix (async I/O to avoid blocking the runtime)
        tokio::fs::create_dir_all(files_dir).await?;
        let uuid = uuid::Uuid::new_v4();
        let safe_filename = sanitize_filename(&url_filename);
        let disk_filename = format!("{uuid}_{safe_filename}");
        let file_path = files_dir.join(&disk_filename);
        tokio::fs::write(&file_path, &bytes).await?;

        info!(
            "Downloaded {} ({}, {}) → {}",
            url_filename,
            mime_type,
            format_size(size),
            file_path.display()
        );

        Ok(DownloadedFile {
            path: file_path,
            filename: url_filename,
            mime_type,
            size,
            category,
        })
    }
}

/// Converts a downloaded file to an Anthropic API content block.
///
/// Returns `Some(InputContentBlock)` for supported types (images, PDFs),
/// or `None` for unsupported types.
///
/// Uses `tokio::fs::read` to avoid blocking the async runtime on large files.
pub async fn file_to_content_block(file: &DownloadedFile) -> Result<Option<InputContentBlock>> {
    match file.category {
        FileCategory::Image => {
            let data = tokio::fs::read(&file.path).await?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
            debug!(
                "Encoded image {} ({}) as base64 ({} chars)",
                file.filename,
                file.mime_type,
                encoded.len()
            );
            Ok(Some(InputContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".to_string(),
                    media_type: file.mime_type.clone(),
                    data: encoded,
                },
            }))
        }
        FileCategory::Document => {
            let data = tokio::fs::read(&file.path).await?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
            debug!(
                "Encoded document {} ({}) as base64 ({} chars)",
                file.filename,
                file.mime_type,
                encoded.len()
            );
            Ok(Some(InputContentBlock::Document {
                source: DocumentSource {
                    source_type: "base64".to_string(),
                    media_type: file.mime_type.clone(),
                    data: encoded,
                },
            }))
        }
        FileCategory::Other => {
            warn!(
                "Unsupported file type {} — stored but not sent to LLM",
                file.mime_type
            );
            Ok(None)
        }
    }
}

/// Categorizes a MIME type into a file category.
fn categorize_mime(mime: &str) -> FileCategory {
    match mime {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" => FileCategory::Image,
        "application/pdf" => FileCategory::Document,
        _ => FileCategory::Other,
    }
}

/// Guesses MIME type from a filename extension.
fn mime_from_extension(filename: &str) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Sanitizes a filename for safe disk storage.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Formats a byte size as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", bytes / 1024)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_categorize_mime_images() {
        assert_eq!(categorize_mime("image/jpeg"), FileCategory::Image);
        assert_eq!(categorize_mime("image/png"), FileCategory::Image);
        assert_eq!(categorize_mime("image/gif"), FileCategory::Image);
        assert_eq!(categorize_mime("image/webp"), FileCategory::Image);
    }

    #[test]
    fn test_categorize_mime_documents() {
        assert_eq!(categorize_mime("application/pdf"), FileCategory::Document);
    }

    #[test]
    fn test_categorize_mime_other() {
        assert_eq!(categorize_mime("text/plain"), FileCategory::Other);
        assert_eq!(categorize_mime("video/mp4"), FileCategory::Other);
        assert_eq!(
            categorize_mime("application/octet-stream"),
            FileCategory::Other
        );
    }

    #[test]
    fn test_mime_from_extension() {
        assert_eq!(mime_from_extension("photo.jpg"), "image/jpeg");
        assert_eq!(mime_from_extension("photo.jpeg"), "image/jpeg");
        assert_eq!(mime_from_extension("image.png"), "image/png");
        assert_eq!(mime_from_extension("anim.gif"), "image/gif");
        assert_eq!(mime_from_extension("photo.webp"), "image/webp");
        assert_eq!(mime_from_extension("doc.pdf"), "application/pdf");
        assert_eq!(mime_from_extension("readme.txt"), "text/plain");
        assert_eq!(mime_from_extension("unknown.xyz"), "application/octet-stream");
        assert_eq!(mime_from_extension("noext"), "application/octet-stream");
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_filename("my file (1).pdf"), "my_file__1_.pdf");
        assert_eq!(sanitize_filename("a/b/c.txt"), "a_b_c.txt");
        assert_eq!(sanitize_filename("test-file_v2.png"), "test-file_v2.png");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1024), "1KB");
        assert_eq!(format_size(1536), "1KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5MB");
    }

    #[test]
    fn test_downloaded_file_human_size() {
        let file = DownloadedFile {
            path: PathBuf::from("/tmp/test.jpg"),
            filename: "test.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            size: 45 * 1024,
            category: FileCategory::Image,
        };
        assert_eq!(file.human_size(), "45KB");
    }

    #[tokio::test]
    async fn test_file_to_content_block_image() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.jpg");
        // Write some fake image data
        std::fs::write(&file_path, b"fake jpeg data").unwrap();

        let file = DownloadedFile {
            path: file_path,
            filename: "test.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            size: 14,
            category: FileCategory::Image,
        };

        let block = file_to_content_block(&file).await.unwrap().unwrap();
        match block {
            InputContentBlock::Image { source } => {
                assert_eq!(source.source_type, "base64");
                assert_eq!(source.media_type, "image/jpeg");
                // Verify base64 encoding
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&source.data)
                    .unwrap();
                assert_eq!(decoded, b"fake jpeg data");
            }
            _ => panic!("Expected Image content block"),
        }
    }

    #[tokio::test]
    async fn test_file_to_content_block_document() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.pdf");
        std::fs::write(&file_path, b"fake pdf data").unwrap();

        let file = DownloadedFile {
            path: file_path,
            filename: "test.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: 13,
            category: FileCategory::Document,
        };

        let block = file_to_content_block(&file).await.unwrap().unwrap();
        match block {
            InputContentBlock::Document { source } => {
                assert_eq!(source.source_type, "base64");
                assert_eq!(source.media_type, "application/pdf");
            }
            _ => panic!("Expected Document content block"),
        }
    }

    #[tokio::test]
    async fn test_file_to_content_block_other_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.mp4");
        std::fs::write(&file_path, b"fake video data").unwrap();

        let file = DownloadedFile {
            path: file_path,
            filename: "test.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            size: 15,
            category: FileCategory::Other,
        };

        let block = file_to_content_block(&file).await.unwrap();
        assert!(block.is_none());
    }

    #[tokio::test]
    async fn test_downloader_rejects_http_urls() {
        let downloader = FileDownloader::new(1);
        let dir = tempfile::tempdir().unwrap();

        let result = downloader
            .download("http://example.com/file.jpg", dir.path())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HTTPS"));
    }

    #[tokio::test]
    async fn test_downloader_rejects_invalid_urls() {
        let downloader = FileDownloader::new(1);
        let dir = tempfile::tempdir().unwrap();

        let result = downloader.download("not-a-url", dir.path()).await;
        assert!(result.is_err());
    }
}
