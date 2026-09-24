//! Load image tool for loading template/mask images from disk.
//!
//! This module implements the `load_image` MCP tool which loads images from
//! local file paths, optionally processes them, and stores them in a cache
//! for later use with `find_image`.

use crate::tools::image_cache::{
    ImageCache, ImageMetadata, MAX_IMAGE_DIMENSION, MAX_IMAGE_FILE_SIZE,
};
use base64::Engine;
use image::{DynamicImage, GenericImageView, ImageReader};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Parameters for the load_image tool.
#[derive(Debug, Deserialize)]
pub struct LoadImageParams {
    /// Local filesystem path to the image file.
    pub path: String,

    /// Optional prefix for the generated ID (e.g., "template", "mask").
    pub id_prefix: Option<String>,

    /// Maximum width to downscale to (maintains aspect ratio).
    pub max_width: Option<u32>,

    /// Maximum height to downscale to (maintains aspect ratio).
    pub max_height: Option<u32>,

    /// If true, convert the image to a single-channel grayscale mask.
    #[serde(default)]
    pub as_mask: bool,

    /// If true, include base64-encoded image data in the response.
    #[serde(default)]
    pub return_base64: bool,
}

/// Response from the load_image tool.
#[derive(Debug, Serialize, Deserialize)]
pub struct LoadImageResponse {
    /// Unique ID for this image in the cache.
    pub image_id: String,
    /// Image width in pixels (after any processing).
    pub width: u32,
    /// Image height in pixels (after any processing).
    pub height: u32,
    /// Number of channels (1 for grayscale/mask, 3 for RGB, 4 for RGBA).
    pub channels: u8,
    /// MIME type of the cached image (always "image/png" after processing).
    pub mime: String,
    /// SHA-256 hash of the original file bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Base64-encoded image data (if return_base64 was true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

/// Input data for the blocking image processing operation.
struct ProcessingInput {
    path: String,
    max_width: Option<u32>,
    max_height: Option<u32>,
    as_mask: bool,
    return_base64: bool,
}

/// Result from the blocking processing operation.
struct ProcessingResult {
    png_data: Vec<u8>,
    metadata: ImageMetadata,
    sha256: String,
    base64: Option<String>,
}

/// Execute the load_image tool.
pub async fn load_image(params: LoadImageParams, cache: Arc<RwLock<ImageCache>>) -> CallToolResult {
    // Validate max_width/max_height are positive if provided
    if let Some(0) = params.max_width {
        return CallToolResult::error(vec![ContentBlock::text(
            "max_width must be greater than 0".to_string(),
        )]);
    }
    if let Some(0) = params.max_height {
        return CallToolResult::error(vec![ContentBlock::text(
            "max_height must be greater than 0".to_string(),
        )]);
    }

    // Prepare input for blocking operation (all file I/O happens in spawn_blocking)
    let input = ProcessingInput {
        path: params.path.clone(),
        max_width: params.max_width,
        max_height: params.max_height,
        as_mask: params.as_mask,
        return_base64: params.return_base64,
    };

    // Move heavy CPU work to a blocking thread
    let result = match tokio::task::spawn_blocking(move || process_image(input)).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => return CallToolResult::error(vec![ContentBlock::text(e)]),
        Err(e) => {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Task panicked: {}",
                e
            ))]);
        }
    };

    // Store in cache
    let image_id = {
        let mut cache_guard = cache.write().await;
        cache_guard.store(
            result.png_data,
            result.metadata.clone(),
            params.id_prefix.as_deref(),
        )
    };

    // Build response
    let response = LoadImageResponse {
        image_id,
        width: result.metadata.width,
        height: result.metadata.height,
        channels: result.metadata.channels,
        mime: result.metadata.mime,
        sha256: Some(result.sha256),
        base64: result.base64,
    };

    match serde_json::to_string_pretty(&response) {
        Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
            "Failed to serialize response: {}",
            e
        ))]),
    }
}

/// CPU-intensive image processing logic, runs on a blocking thread.
/// All file I/O is performed here to avoid blocking the async runtime.
fn process_image(input: ProcessingInput) -> Result<ProcessingResult, String> {
    let path = Path::new(&input.path);

    // Validate path exists and is a file
    if !path.exists() {
        return Err(format!("File not found: {}", input.path));
    }
    if !path.is_file() {
        return Err(format!("Path is not a file: {}", input.path));
    }

    // Check file size before reading
    let file_size = fs::metadata(path)
        .map_err(|e| format!("Failed to read file metadata: {}", e))?
        .len();

    if file_size > MAX_IMAGE_FILE_SIZE {
        return Err(format!(
            "File too large: {} bytes (max {} bytes)",
            file_size, MAX_IMAGE_FILE_SIZE
        ));
    }

    // Read file bytes
    let file_bytes = fs::read(path).map_err(|e| format!("Failed to read file: {}", e))?;

    // Compute SHA-256 of original file
    let sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(&file_bytes);
        format!("{:x}", hasher.finalize())
    };

    // Check dimensions BEFORE full decode to avoid memory exhaustion
    let reader = ImageReader::new(Cursor::new(&file_bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to detect image format: {}", e))?;

    let (orig_width, orig_height) = reader
        .into_dimensions()
        .map_err(|e| format!("Failed to read image dimensions: {}", e))?;

    // Validate dimensions before decoding
    if orig_width > MAX_IMAGE_DIMENSION || orig_height > MAX_IMAGE_DIMENSION {
        return Err(format!(
            "Image dimensions too large: {}x{} (max {}x{})",
            orig_width, orig_height, MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION
        ));
    }

    // Now decode the full image (dimensions are safe)
    let img = ImageReader::new(Cursor::new(&file_bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to detect image format: {}", e))?
        .decode()
        .map_err(|e| format!("Failed to decode image: {}", e))?;

    // Apply optional downscaling
    let img = apply_max_dimensions(img, input.max_width, input.max_height);
    let (width, height) = img.dimensions();

    // Convert to mask if requested
    let (final_img, channels) = if input.as_mask {
        let gray = img.to_luma8();
        (DynamicImage::ImageLuma8(gray), 1u8)
    } else {
        // Keep original color model
        let channels = match &img {
            DynamicImage::ImageLuma8(_) | DynamicImage::ImageLuma16(_) => 1,
            DynamicImage::ImageLumaA8(_) | DynamicImage::ImageLumaA16(_) => 2,
            DynamicImage::ImageRgb8(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgb32F(_) => 3,
            DynamicImage::ImageRgba8(_)
            | DynamicImage::ImageRgba16(_)
            | DynamicImage::ImageRgba32F(_) => 4,
            _ => 4, // Default to RGBA for unknown formats
        };
        (img, channels)
    };

    // Encode as PNG
    let mut png_data = Vec::new();
    let mut cursor = Cursor::new(&mut png_data);
    final_img
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode PNG: {}", e))?;

    // Optionally encode base64
    let base64 = if input.return_base64 {
        Some(base64::engine::general_purpose::STANDARD.encode(&png_data))
    } else {
        None
    };

    let metadata = ImageMetadata {
        source_path: Some(input.path),
        width,
        height,
        channels,
        mime: "image/png".to_string(),
        sha256: Some(sha256.clone()),
        is_mask: input.as_mask,
    };

    Ok(ProcessingResult {
        png_data,
        metadata,
        sha256,
        base64,
    })
}

/// Apply max_width and max_height constraints while maintaining aspect ratio.
fn apply_max_dimensions(
    img: DynamicImage,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> DynamicImage {
    let (orig_w, orig_h) = img.dimensions();

    let scale_w = max_width.map(|mw| mw as f64 / orig_w as f64).unwrap_or(1.0);
    let scale_h = max_height
        .map(|mh| mh as f64 / orig_h as f64)
        .unwrap_or(1.0);

    let scale = scale_w.min(scale_h);

    if scale >= 1.0 {
        // No need to downscale
        return img;
    }

    let new_w = ((orig_w as f64) * scale).round() as u32;
    let new_h = ((orig_h as f64) * scale).round() as u32;

    // Ensure at least 1x1
    let new_w = new_w.max(1);
    let new_h = new_h.max(1);

    img.resize(new_w, new_h, image::imageops::FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // Helper to create a test PNG file
    fn create_test_png(width: u32, height: u32) -> NamedTempFile {
        let img = DynamicImage::new_rgb8(width, height);
        let mut file = NamedTempFile::new().unwrap();
        let mut cursor = Cursor::new(Vec::new());
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        file.write_all(cursor.get_ref()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_apply_max_dimensions_both_constraints() {
        let img = DynamicImage::new_rgb8(400, 200);

        // Width is more constraining (400 -> 100 = 0.25 scale)
        // Height would be (200 -> 200 = 1.0 scale)
        let result = apply_max_dimensions(img, Some(100), Some(200));
        assert_eq!(result.dimensions(), (100, 50));
    }

    #[test]
    fn test_apply_max_dimensions_minimum_size() {
        let img = DynamicImage::new_rgb8(100, 100);

        // Very small constraint should still produce at least 1x1
        let result = apply_max_dimensions(img, Some(1), Some(1));
        assert!(result.width() >= 1);
        assert!(result.height() >= 1);
    }

    #[test]
    fn test_process_image_basic() {
        let file = create_test_png(64, 64);

        let input = ProcessingInput {
            path: file.path().to_string_lossy().to_string(),
            max_width: None,
            max_height: None,
            as_mask: false,
            return_base64: false,
        };

        let result = process_image(input).unwrap();

        assert_eq!(result.metadata.width, 64);
        assert_eq!(result.metadata.height, 64);
        assert_eq!(result.metadata.channels, 3); // RGB
        assert_eq!(result.metadata.mime, "image/png");
        assert!(result.metadata.sha256.is_some());
        assert!(!result.png_data.is_empty());
        assert!(result.base64.is_none());
    }

    #[test]
    fn test_process_image_with_base64() {
        let file = create_test_png(32, 32);

        let input = ProcessingInput {
            path: file.path().to_string_lossy().to_string(),
            max_width: None,
            max_height: None,
            as_mask: false,
            return_base64: true,
        };

        let result = process_image(input).unwrap();

        assert!(result.base64.is_some());
        let b64 = result.base64.unwrap();
        // Verify it's valid base64 that decodes to PNG
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(decoded, result.png_data);
    }

    #[test]
    fn test_process_image_as_mask() {
        let file = create_test_png(48, 48);

        let input = ProcessingInput {
            path: file.path().to_string_lossy().to_string(),
            max_width: None,
            max_height: None,
            as_mask: true,
            return_base64: false,
        };

        let result = process_image(input).unwrap();

        assert_eq!(result.metadata.channels, 1); // Grayscale
        assert!(result.metadata.is_mask);
    }

    #[test]
    fn test_process_image_with_downscale() {
        let file = create_test_png(200, 100);

        let input = ProcessingInput {
            path: file.path().to_string_lossy().to_string(),
            max_width: Some(100),
            max_height: Some(100),
            as_mask: false,
            return_base64: false,
        };

        let result = process_image(input).unwrap();

        // 200x100 with max 100x100 should scale to 100x50
        assert_eq!(result.metadata.width, 100);
        assert_eq!(result.metadata.height, 50);
    }

    #[tokio::test]
    async fn test_load_image_file_not_found() {
        let cache = Arc::new(RwLock::new(ImageCache::default()));

        let params = LoadImageParams {
            path: "/nonexistent/path/to/image.png".to_string(),
            id_prefix: None,
            max_width: None,
            max_height: None,
            as_mask: false,
            return_base64: false,
        };

        let result = load_image(params, cache).await;
        assert!(result.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn test_load_image_stores_in_cache() {
        let cache = Arc::new(RwLock::new(ImageCache::default()));
        let file = create_test_png(32, 32);

        let params = LoadImageParams {
            path: file.path().to_string_lossy().to_string(),
            id_prefix: Some("template".to_string()),
            max_width: None,
            max_height: None,
            as_mask: false,
            return_base64: false,
        };

        let result = load_image(params, cache.clone()).await;
        assert!(!result.is_error.unwrap_or(true));

        // Parse the response to get image_id
        let content = &result.content[0];
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) = &content {
            let response: LoadImageResponse = serde_json::from_str(text).unwrap();
            assert!(response.image_id.starts_with("template-"));

            // Verify it's in the cache
            let cache_guard = cache.read().await;
            assert!(cache_guard.contains(&response.image_id));
        } else {
            panic!("Expected text content");
        }
    }

    #[tokio::test]
    async fn test_load_image_not_a_file() {
        let cache = Arc::new(RwLock::new(ImageCache::default()));
        let dir = tempfile::tempdir().unwrap();

        let params = LoadImageParams {
            path: dir.path().to_string_lossy().to_string(),
            id_prefix: None,
            max_width: None,
            max_height: None,
            as_mask: false,
            return_base64: false,
        };

        let result = load_image(params, cache).await;
        assert!(result.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn test_load_image_zero_max_width() {
        let cache = Arc::new(RwLock::new(ImageCache::default()));

        let params = LoadImageParams {
            path: "/some/path.png".to_string(),
            id_prefix: None,
            max_width: Some(0),
            max_height: None,
            as_mask: false,
            return_base64: false,
        };

        let result = load_image(params, cache).await;
        assert!(result.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn test_load_image_zero_max_height() {
        let cache = Arc::new(RwLock::new(ImageCache::default()));

        let params = LoadImageParams {
            path: "/some/path.png".to_string(),
            id_prefix: None,
            max_width: None,
            max_height: Some(0),
            as_mask: false,
            return_base64: false,
        };

        let result = load_image(params, cache).await;
        assert!(result.is_error.unwrap_or(false));
    }
}
