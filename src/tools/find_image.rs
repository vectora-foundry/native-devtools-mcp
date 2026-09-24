//! Template matching tool for locating images within screenshots.
//!
//! This module implements the `find_image` MCP tool which uses normalized
//! cross-correlation (NCC) to find template images within screenshots.
//!
//! ## Performance Optimizations
//!
//! This module supports several optional performance optimizations:
//!
//! - **Algorithmic**: Dynamic downscaling in fast mode, early exit on high-confidence
//!   matches, and efficient scale loop termination.
//! - **Parallelism** (`find_image_parallel` feature): Uses Rayon to process
//!   scale/rotation combinations in parallel.
//! - **SIMD** (`find_image_simd` feature): Uses the `wide` crate for vectorized
//!   NCC computation on x86_64 and aarch64.

use crate::tools::image_cache::ImageCache;
use crate::tools::screenshot_cache::{ScreenshotCache, ScreenshotMetadata};
use base64::Engine;
use image::{GrayImage, ImageReader};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::RwLock;

#[cfg(feature = "find_image_parallel")]
use rayon::prelude::*;

#[cfg(feature = "find_image_parallel")]
use std::sync::OnceLock;

/// Static thread pool for parallel find_image operations.
/// Uses half of available CPUs to avoid oversubscribing when running inside spawn_blocking.
#[cfg(feature = "find_image_parallel")]
static FIND_IMAGE_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

#[cfg(feature = "find_image_parallel")]
fn get_thread_pool() -> &'static rayon::ThreadPool {
    FIND_IMAGE_POOL.get_or_init(|| {
        let num_threads = (rayon::current_num_threads() / 2).max(1);
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .expect("failed to create rayon thread pool")
    })
}

/// Parameters for the find_image tool.
#[derive(Debug, Deserialize)]
pub struct FindImageParams {
    /// Screenshot ID from a previous take_screenshot call.
    pub screenshot_id: Option<String>,

    /// Base64-encoded screenshot image (used if no screenshot_id).
    pub screenshot_image_base64: Option<String>,

    /// Image ID from a previous load_image call (preferred over template_image_base64).
    pub template_id: Option<String>,

    /// Base64-encoded template image to find (used if no template_id).
    pub template_image_base64: Option<String>,

    /// Image ID from a previous load_image call for the mask.
    pub mask_id: Option<String>,

    /// Base64-encoded mask image (optional; white=match, black=ignore).
    pub mask_image_base64: Option<String>,

    /// Mode: "fast" (default) or "accurate".
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Minimum match score threshold (default depends on mode).
    pub threshold: Option<f64>,

    /// Maximum number of results to return (default depends on mode).
    pub max_results: Option<usize>,

    /// Scale search range: {min, max, step}.
    pub scales: Option<ScaleRange>,

    /// Rotations to try in degrees. Only 0, 90, 180, 270 are supported.
    pub rotations: Option<Vec<f64>>,

    /// Search region within the screenshot: {x, y, w, h}.
    pub search_region: Option<SearchRegion>,

    /// Stride for matching (default: 2 in fast mode, 1 in accurate mode).
    pub stride: Option<u32>,

    /// Return screen coordinates (requires screenshot metadata).
    #[serde(default = "default_return_screen_coords")]
    pub return_screen_coords: bool,
}

fn default_mode() -> String {
    "fast".to_string()
}

fn default_return_screen_coords() -> bool {
    true
}

/// Scale search range configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScaleRange {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

impl Default for ScaleRange {
    fn default() -> Self {
        Self {
            min: 0.8,
            max: 1.2,
            step: 0.1,
        }
    }
}

/// Search region within the screenshot.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchRegion {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// A single match result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    /// Match confidence score (0.0 to 1.0).
    pub score: f64,
    /// Bounding box in screenshot pixels.
    pub bbox: BoundingBox,
    /// Center point in screenshot pixels.
    pub center: Point,
    /// Scale at which the match was found.
    pub scale: f64,
    /// Rotation at which the match was found (degrees).
    pub rotation: f64,
    /// Screen X coordinate (if metadata available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_x: Option<f64>,
    /// Screen Y coordinate (if metadata available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_y: Option<f64>,
}

/// Bounding box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Point coordinate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Response from find_image tool.
#[derive(Debug, Serialize, Deserialize)]
pub struct FindImageResponse {
    pub matches: Vec<MatchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Mode-specific defaults.
struct ModeDefaults {
    threshold: f64,
    max_results: usize,
    scales: ScaleRange,
    stride: u32,
}

impl ModeDefaults {
    fn fast() -> Self {
        Self {
            threshold: 0.75,
            max_results: 3,
            scales: ScaleRange {
                min: 0.8,
                max: 1.2,
                step: 0.1,
            },
            stride: 2,
        }
    }

    fn accurate() -> Self {
        Self {
            threshold: 0.75,
            max_results: 5,
            scales: ScaleRange {
                min: 0.5,
                max: 2.0,
                step: 0.05,
            },
            stride: 1,
        }
    }

    fn for_mode(mode: &str) -> Self {
        match mode {
            "accurate" => Self::accurate(),
            _ => Self::fast(),
        }
    }
}

/// Normalize a rotation angle to one of the supported values (0, 90, 180, 270).
/// Returns None if the angle is not within ±1° tolerance of a supported value.
fn normalize_rotation(r: f64) -> Option<f64> {
    let normalized = ((r % 360.0) + 360.0) % 360.0;
    // ±1° tolerance (inclusive)
    if normalized <= 1.0 || normalized >= 359.0 {
        Some(0.0)
    } else if (normalized - 90.0).abs() <= 1.0 {
        Some(90.0)
    } else if (normalized - 180.0).abs() <= 1.0 {
        Some(180.0)
    } else if (normalized - 270.0).abs() <= 1.0 {
        Some(270.0)
    } else {
        None
    }
}

/// Validate scale range parameters.
/// Returns Ok(()) if valid, Err(message) if invalid.
fn validate_scale_range(scales: &ScaleRange) -> Result<(), String> {
    if scales.step <= 0.0 {
        return Err("scales.step must be positive (got 0 or negative)".to_string());
    }
    if scales.min <= 0.0 {
        return Err(format!("scales.min must be positive (got {})", scales.min));
    }
    if scales.max <= 0.0 {
        return Err(format!("scales.max must be positive (got {})", scales.max));
    }
    if scales.min > scales.max {
        return Err(format!(
            "scales.min ({}) must not exceed scales.max ({})",
            scales.min, scales.max
        ));
    }
    Ok(())
}

/// Input data for the blocking matching operation.
struct MatchingInput {
    screenshot_png_data: Option<Vec<u8>>,
    screenshot_b64: Option<String>,
    template_png_data: Option<Vec<u8>>,
    template_b64: Option<String>,
    mask_png_data: Option<Vec<u8>>,
    mask_b64: Option<String>,
    search_region: Option<SearchRegion>,
    threshold: f64,
    max_results: usize,
    scales: ScaleRange,
    stride: u32,
    rotations: Vec<f64>,
    return_screen_coords: bool,
    screenshot_metadata: Option<ScreenshotMetadata>,
    /// Whether this is "fast" mode (enables downscaling and early exit).
    is_fast_mode: bool,
}

/// Work item for parallel processing of scale/rotation combinations.
#[derive(Clone)]
struct WorkItem {
    rotation: f64,
    rotation_idx: usize,
    scale: f64,
}

/// Pre-rotated template and mask for a specific rotation angle.
struct RotatedTemplates {
    template: GrayImage,
    mask: Option<GrayImage>,
}

/// Result from the blocking matching operation.
enum MatchingResult {
    Success(Vec<MatchResult>),
    Error(String),
}

/// Execute the find_image tool.
pub async fn find_image(
    params: FindImageParams,
    screenshot_cache: Arc<RwLock<ScreenshotCache>>,
    image_cache: Arc<RwLock<ImageCache>>,
) -> CallToolResult {
    let defaults = ModeDefaults::for_mode(&params.mode);
    let mut warning: Option<String> = None;

    // Get parameters with mode defaults (cheap, keep on async thread)
    let threshold = params.threshold.unwrap_or(defaults.threshold);
    let max_results = params.max_results.unwrap_or(defaults.max_results);
    let scales = params.scales.clone().unwrap_or(defaults.scales);
    let stride = params.stride.unwrap_or(defaults.stride);
    let rotations = params.rotations.clone().unwrap_or_else(|| vec![0.0]);

    // Validate scale range to prevent infinite loops and degenerate cases
    if let Err(e) = validate_scale_range(&scales) {
        return CallToolResult::error(vec![ContentBlock::text(e)]);
    }

    // Validate, filter, and normalize rotations to exact {0, 90, 180, 270}
    let mut normalized_rotations = Vec::new();
    let mut invalid_rotations = Vec::new();

    for r in rotations {
        match normalize_rotation(r) {
            Some(exact) => {
                if !normalized_rotations.contains(&exact) {
                    normalized_rotations.push(exact);
                }
            }
            None => invalid_rotations.push(r),
        }
    }

    // Treat empty rotations as [0]
    let rotations = if normalized_rotations.is_empty() {
        vec![0.0]
    } else {
        normalized_rotations
    };

    // Warn about unsupported rotations
    if !invalid_rotations.is_empty() {
        let msg = format!(
            "Unsupported rotation angles ignored (only 0, 90, 180, 270 supported): {:?}",
            invalid_rotations
        );
        warning = Some(warning.map_or(msg.clone(), |w| format!("{}; {}", w, msg)));
    }

    // Validate search_region dimensions
    if let Some(region) = &params.search_region {
        if region.w == 0 || region.h == 0 {
            return CallToolResult::error(vec![ContentBlock::text(
                "search_region width and height must be positive",
            )]);
        }
    }

    // Resolve screenshot data from cache - clone bytes and release lock before decode
    let (screenshot_png_data, screenshot_metadata) = {
        if let Some(id) = &params.screenshot_id {
            let cache_guard = screenshot_cache.read().await;
            if let Some(cached) = cache_guard.peek(id) {
                // Clone the data so we can release the lock
                (Some(cached.png_data.clone()), Some(cached.metadata.clone()))
            } else {
                warning = Some(format!("Screenshot ID '{}' not found in cache", id));
                (None, None)
            }
        } else {
            (None, None)
        }
    };
    // Lock is now released

    // Resolve template data from image cache
    // Use write lock to update LRU access order via get()
    let template_png_data = {
        if let Some(id) = &params.template_id {
            let mut cache_guard = image_cache.write().await;
            if let Some(cached) = cache_guard.get(id) {
                Some(cached.png_data.clone())
            } else if params.template_image_base64.is_some() {
                // ID not found but base64 fallback available - warn and continue
                let msg = format!(
                    "Template ID '{}' not found in cache, using base64 fallback",
                    id
                );
                warning = Some(warning.map_or(msg.clone(), |w| format!("{}; {}", w, msg)));
                None
            } else {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Template ID '{}' not found in image cache",
                    id
                ))]);
            }
        } else {
            None
        }
    };

    // Resolve mask data from image cache
    // Use write lock to update LRU access order via get()
    let mask_png_data = {
        if let Some(id) = &params.mask_id {
            let mut cache_guard = image_cache.write().await;
            if let Some(cached) = cache_guard.get(id) {
                Some(cached.png_data.clone())
            } else if params.mask_image_base64.is_some() {
                // ID not found but base64 fallback available - warn and continue
                let msg = format!("Mask ID '{}' not found in cache, using base64 fallback", id);
                warning = Some(warning.map_or(msg.clone(), |w| format!("{}; {}", w, msg)));
                None
            } else {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Mask ID '{}' not found in image cache",
                    id
                ))]);
            }
        } else {
            None
        }
    };

    // Validate that we have a template source
    if params.template_id.is_none() && params.template_image_base64.is_none() {
        return CallToolResult::error(vec![ContentBlock::text(
            "Either template_id or template_image_base64 must be provided",
        )]);
    }

    // Prepare input for blocking operation
    let is_fast_mode = params.mode != "accurate";
    let input = MatchingInput {
        screenshot_png_data,
        screenshot_b64: params.screenshot_image_base64.clone(),
        template_png_data,
        template_b64: params.template_image_base64.clone(),
        mask_png_data,
        mask_b64: params.mask_image_base64.clone(),
        search_region: params.search_region.clone(),
        threshold,
        max_results,
        scales,
        stride,
        rotations,
        return_screen_coords: params.return_screen_coords,
        screenshot_metadata,
        is_fast_mode,
    };

    // Move heavy CPU work to a blocking thread
    let result = tokio::task::spawn_blocking(move || run_matching(input))
        .await
        .unwrap_or_else(|e| MatchingResult::Error(format!("Task panicked: {}", e)));

    match result {
        MatchingResult::Success(matches) => {
            let response = FindImageResponse { matches, warning };
            match serde_json::to_string_pretty(&response) {
                Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
                Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
                    "Failed to serialize response: {}",
                    e
                ))]),
            }
        }
        MatchingResult::Error(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
    }
}

/// Compute the downscale factor for fast mode.
///
/// In fast mode, if the search image max dimension exceeds 1200px, we downscale
/// to reduce NCC computation. The downscale factor is capped at 0.5 to avoid
/// losing too much detail.
fn compute_downscale_factor(search_img: &GrayImage, _template: &GrayImage) -> f64 {
    let max_dim = search_img.width().max(search_img.height()) as f64;
    const TARGET_MAX_DIM: f64 = 1200.0;
    const MIN_DOWNSCALE: f64 = 0.5;

    if max_dim <= TARGET_MAX_DIM {
        1.0
    } else {
        (TARGET_MAX_DIM / max_dim).max(MIN_DOWNSCALE)
    }
}

/// Build a list of work items from rotations and scales, pruning scales that
/// would make the template larger than the search image.
fn build_work_items(
    rotations: &[f64],
    scales: &ScaleRange,
    rotated_templates: &[RotatedTemplates],
    search_img: &GrayImage,
) -> Vec<WorkItem> {
    let mut items = Vec::new();
    for (rotation_idx, &rotation) in rotations.iter().enumerate() {
        let tpl = &rotated_templates[rotation_idx].template;
        let max_scale_w = search_img.width() as f64 / tpl.width() as f64;
        let max_scale_h = search_img.height() as f64 / tpl.height() as f64;
        let max_scale = max_scale_w.min(max_scale_h);

        let mut scale = scales.min;
        while scale <= scales.max + f64::EPSILON && scale <= max_scale + f64::EPSILON {
            items.push(WorkItem {
                rotation,
                rotation_idx,
                scale,
            });
            scale += scales.step;
        }
    }
    items
}

/// Pre-compute rotated templates for each unique rotation angle.
/// Returns a Vec indexed by rotation_idx.
fn build_rotated_templates(
    template: &GrayImage,
    mask: Option<&GrayImage>,
    rotations: &[f64],
) -> Vec<RotatedTemplates> {
    rotations
        .iter()
        .map(|&rotation| RotatedTemplates {
            template: rotate_image(template, rotation),
            mask: mask.map(|m| rotate_image(m, rotation)),
        })
        .collect()
}

/// Process a single work item (rotation + scale combination).
/// Returns matches for this specific configuration.
/// The `rotated_template` and `rotated_mask` should be pre-rotated for this work item's rotation.
#[allow(clippy::too_many_arguments)]
fn process_work_item(
    item: &WorkItem,
    search_img: &GrayImage,
    rotated_template: &GrayImage,
    rotated_mask: Option<&GrayImage>,
    threshold: f64,
    stride: u32,
    region_offset: (u32, u32),
    downscale_factor: f64,
    screenshot_metadata: Option<&ScreenshotMetadata>,
    return_screen_coords: bool,
) -> Option<Vec<MatchResult>> {
    // Scale the pre-rotated template and mask
    let scaled_template = resize_image(rotated_template, item.scale);
    let scaled_mask = rotated_mask.map(|m| resize_image(m, item.scale));

    // Check if template fits in search image
    if scaled_template.width() > search_img.width()
        || scaled_template.height() > search_img.height()
    {
        return None; // Template too large
    }

    // Run NCC matching
    let matches = match_template_ncc(
        search_img,
        &scaled_template,
        scaled_mask.as_ref(),
        threshold,
        stride,
    );

    if matches.is_empty() {
        return Some(Vec::new());
    }

    // Convert to MatchResult with adjusted coordinates
    let results: Vec<MatchResult> = matches
        .into_iter()
        .map(|(x, y, score)| {
            // Map coordinates back from downscaled space to original space
            let full_x = if downscale_factor < 1.0 {
                (x as f64 / downscale_factor).round() as u32
            } else {
                x
            };
            let full_y = if downscale_factor < 1.0 {
                (y as f64 / downscale_factor).round() as u32
            } else {
                y
            };
            let full_tw = if downscale_factor < 1.0 {
                (scaled_template.width() as f64 / downscale_factor).round() as u32
            } else {
                scaled_template.width()
            };
            let full_th = if downscale_factor < 1.0 {
                (scaled_template.height() as f64 / downscale_factor).round() as u32
            } else {
                scaled_template.height()
            };

            let adjusted_x = full_x + region_offset.0;
            let adjusted_y = full_y + region_offset.1;

            let center_x = adjusted_x as f64 + full_tw as f64 / 2.0;
            let center_y = adjusted_y as f64 + full_th as f64 / 2.0;

            // Convert to screen coordinates if metadata available
            let (screen_x, screen_y) = if return_screen_coords {
                if let Some(meta) = screenshot_metadata {
                    let sx = meta.origin_x + center_x / meta.scale;
                    let sy = meta.origin_y + center_y / meta.scale;
                    (Some(sx), Some(sy))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

            MatchResult {
                score,
                bbox: BoundingBox {
                    x: adjusted_x,
                    y: adjusted_y,
                    w: full_tw,
                    h: full_th,
                },
                center: Point {
                    x: center_x,
                    y: center_y,
                },
                scale: item.scale,
                rotation: item.rotation,
                screen_x,
                screen_y,
            }
        })
        .collect();

    Some(results)
}

/// CPU-intensive matching logic, runs on a blocking thread.
fn run_matching(input: MatchingInput) -> MatchingResult {
    // Decode screenshot
    let (screenshot_gray, screenshot_metadata) = if let Some(png_data) = input.screenshot_png_data {
        match decode_png_to_gray(&png_data) {
            Ok(gray) => (gray, input.screenshot_metadata),
            Err(e) => {
                return MatchingResult::Error(format!("Failed to decode cached screenshot: {}", e))
            }
        }
    } else if let Some(b64) = &input.screenshot_b64 {
        match decode_base64_to_gray(b64) {
            Ok(gray) => (gray, None),
            Err(e) => return MatchingResult::Error(format!("Failed to decode screenshot: {}", e)),
        }
    } else {
        return MatchingResult::Error(
            "Either screenshot_id or screenshot_image_base64 must be provided".to_string(),
        );
    };

    // Decode template image (prefer cached PNG data over base64)
    let template_gray = if let Some(png_data) = input.template_png_data {
        match decode_png_to_gray(&png_data) {
            Ok(img) => img,
            Err(e) => {
                return MatchingResult::Error(format!("Failed to decode cached template: {}", e))
            }
        }
    } else if let Some(b64) = &input.template_b64 {
        match decode_base64_to_gray(b64) {
            Ok(img) => img,
            Err(e) => {
                return MatchingResult::Error(format!("Failed to decode template image: {}", e))
            }
        }
    } else {
        return MatchingResult::Error(
            "Either template_id or template_image_base64 must be provided".to_string(),
        );
    };

    // Decode mask if provided (prefer cached PNG data over base64)
    let mask = if let Some(png_data) = input.mask_png_data {
        match decode_png_to_gray(&png_data) {
            Ok(img) => Some(img),
            Err(e) => return MatchingResult::Error(format!("Failed to decode cached mask: {}", e)),
        }
    } else if let Some(mask_b64) = &input.mask_b64 {
        match decode_base64_to_gray(mask_b64) {
            Ok(img) => Some(img),
            Err(e) => return MatchingResult::Error(format!("Failed to decode mask image: {}", e)),
        }
    } else {
        None
    };

    // Validate mask dimensions match template
    if let Some(mask_img) = &mask {
        if mask_img.width() != template_gray.width() || mask_img.height() != template_gray.height()
        {
            return MatchingResult::Error(format!(
                "Mask dimensions ({}x{}) must match template dimensions ({}x{})",
                mask_img.width(),
                mask_img.height(),
                template_gray.width(),
                template_gray.height()
            ));
        }
    }

    // Extract search region if specified (use Cow to avoid cloning large screenshot)
    let (search_img_region, region_offset) = if let Some(region) = &input.search_region {
        (
            Cow::Owned(extract_region(&screenshot_gray, region)),
            (region.x, region.y),
        )
    } else {
        (Cow::Borrowed(&screenshot_gray), (0, 0))
    };

    // Apply dynamic downscale in fast mode
    let downscale_factor = if input.is_fast_mode {
        compute_downscale_factor(&search_img_region, &template_gray)
    } else {
        1.0
    };

    // Prepare images for matching
    let (search_img, template_for_matching, mask_for_matching) = if downscale_factor < 1.0 {
        (
            resize_image(&search_img_region, downscale_factor),
            resize_image(&template_gray, downscale_factor),
            mask.as_ref().map(|m| resize_image(m, downscale_factor)),
        )
    } else {
        (
            search_img_region.into_owned(),
            template_gray.clone(),
            mask.clone(),
        )
    };

    // Pre-compute rotated templates once per rotation, then build pruned work items
    let rotated_templates = build_rotated_templates(
        &template_for_matching,
        mask_for_matching.as_ref(),
        &input.rotations,
    );
    let work_items = build_work_items(
        &input.rotations,
        &input.scales,
        &rotated_templates,
        &search_img,
    );

    // Process work items (parallel or sequential based on feature flag)
    #[cfg(feature = "find_image_parallel")]
    let all_matches: Vec<MatchResult> = {
        let results: Vec<Vec<MatchResult>> = get_thread_pool().install(|| {
            work_items
                .par_iter()
                .filter_map(|item| {
                    let rotated = &rotated_templates[item.rotation_idx];
                    let matches = process_work_item(
                        item,
                        &search_img,
                        &rotated.template,
                        rotated.mask.as_ref(),
                        input.threshold,
                        input.stride,
                        region_offset,
                        downscale_factor,
                        screenshot_metadata.as_ref(),
                        input.return_screen_coords,
                    )?;

                    Some(matches)
                })
                .collect()
        });

        results.into_iter().flatten().collect()
    };

    #[cfg(not(feature = "find_image_parallel"))]
    let all_matches: Vec<MatchResult> = {
        let mut matches = Vec::new();
        let mut high_conf_matches: Vec<MatchResult> = Vec::new();

        // Early exit threshold: stop when we have enough unique high-confidence matches
        let early_exit_threshold = if input.is_fast_mode {
            input.threshold.max(0.95)
        } else {
            1.1 // Effectively disabled in accurate mode
        };

        for item in &work_items {
            let rotated = &rotated_templates[item.rotation_idx];
            match process_work_item(
                item,
                &search_img,
                &rotated.template,
                rotated.mask.as_ref(),
                input.threshold,
                input.stride,
                region_offset,
                downscale_factor,
                screenshot_metadata.as_ref(),
                input.return_screen_coords,
            ) {
                Some(item_matches) => {
                    // Track high-confidence matches for early-exit (after NMS)
                    if input.is_fast_mode {
                        for m in &item_matches {
                            if m.score >= early_exit_threshold {
                                high_conf_matches.push(m.clone());
                            }
                        }
                        if high_conf_matches.len() >= input.max_results {
                            let nms = non_maximum_suppression(
                                high_conf_matches.clone(),
                                0.3,
                                input.max_results,
                            );
                            if nms.len() >= input.max_results {
                                break;
                            }
                        }
                    }
                    matches.extend(item_matches);
                }
                None => {}
            }
        }

        matches
    };

    // Sort by score for deterministic NMS (especially important for parallel execution)
    let mut sorted_matches = all_matches;
    sorted_matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Apply Non-Maximum Suppression
    let final_matches = non_maximum_suppression(sorted_matches, 0.3, input.max_results);

    MatchingResult::Success(final_matches)
}

/// Decode base64 image data to grayscale.
fn decode_base64_to_gray(b64: &str) -> Result<GrayImage, String> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("Invalid base64: {}", e))?;
    decode_png_to_gray(&data)
}

/// Decode PNG/JPEG bytes to grayscale.
fn decode_png_to_gray(data: &[u8]) -> Result<GrayImage, String> {
    let img = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("Failed to read image format: {}", e))?
        .decode()
        .map_err(|e| format!("Failed to decode image: {}", e))?;
    Ok(img.to_luma8())
}

/// Extract a region from an image.
fn extract_region(img: &GrayImage, region: &SearchRegion) -> GrayImage {
    let x = region.x.min(img.width().saturating_sub(1));
    let y = region.y.min(img.height().saturating_sub(1));
    let w = region.w.min(img.width() - x);
    let h = region.h.min(img.height() - y);

    let sub = image::imageops::crop_imm(img, x, y, w, h);
    sub.to_image()
}

/// Resize image by scale factor.
fn resize_image(img: &GrayImage, scale: f64) -> GrayImage {
    if (scale - 1.0).abs() < f64::EPSILON {
        return img.clone();
    }

    let new_width = ((img.width() as f64) * scale).round() as u32;
    let new_height = ((img.height() as f64) * scale).round() as u32;

    if new_width == 0 || new_height == 0 {
        return GrayImage::new(1, 1);
    }

    image::imageops::resize(
        img,
        new_width,
        new_height,
        image::imageops::FilterType::Triangle,
    )
}

/// Simple rotation (only supports 0, 90, 180, 270).
/// Expects normalized input from validation (exact 0.0, 90.0, 180.0, or 270.0).
fn rotate_image(img: &GrayImage, degrees: f64) -> GrayImage {
    // Use rounding to handle any floating point imprecision
    let rounded = degrees.round() as i32;
    match rounded {
        90 => image::imageops::rotate90(img),
        180 => image::imageops::rotate180(img),
        270 => image::imageops::rotate270(img),
        _ => img.clone(), // 0 or fallback
    }
}

/// Normalized Cross-Correlation template matching.
///
/// Returns a list of (x, y, score) for matches above threshold.
///
/// When the `find_image_simd` feature is enabled and no mask is present,
/// this function uses SIMD-accelerated NCC computation for templates
/// with width >= 16 pixels.
///
/// This function is public for benchmarking purposes.
pub fn match_template_ncc(
    image: &GrayImage,
    template: &GrayImage,
    mask: Option<&GrayImage>,
    threshold: f64,
    stride: u32,
) -> Vec<(u32, u32, f64)> {
    let img_w = image.width();
    let img_h = image.height();
    let tpl_w = template.width();
    let tpl_h = template.height();

    if tpl_w > img_w || tpl_h > img_h {
        return Vec::new();
    }

    let stride = stride.max(1);

    // Precompute template statistics
    let tpl_stats = compute_template_stats(template, mask);

    if tpl_stats.std < f64::EPSILON || tpl_stats.pixel_count == 0 {
        return Vec::new();
    }

    let mut matches = Vec::new();

    let search_w = img_w - tpl_w + 1;
    let search_h = img_h - tpl_h + 1;

    // Determine whether to use SIMD path
    #[cfg(feature = "find_image_simd")]
    let use_simd = mask.is_none() && tpl_w >= 16;
    #[cfg(not(feature = "find_image_simd"))]
    let use_simd = false;

    // Iterate over search positions with stride
    let mut y = 0u32;
    while y < search_h {
        let mut x = 0u32;
        while x < search_w {
            let score = if use_simd {
                #[cfg(feature = "find_image_simd")]
                {
                    compute_ncc_at_simd(image, template, x, y, &tpl_stats)
                }
                #[cfg(not(feature = "find_image_simd"))]
                {
                    compute_ncc_at(image, template, mask, x, y, &tpl_stats)
                }
            } else {
                compute_ncc_at(image, template, mask, x, y, &tpl_stats)
            };

            if score >= threshold {
                matches.push((x, y, score));
            }

            x += stride;
        }
        y += stride;
    }

    matches
}

/// Precomputed template statistics for NCC matching.
/// Public for benchmarking purposes.
pub struct TemplateStats {
    /// Template mean pixel value.
    pub mean: f64,
    /// Template standard deviation.
    pub std: f64,
    /// Number of active pixels (respecting mask).
    pub pixel_count: usize,
}

/// Compute template mean, std deviation, and pixel count.
/// Public for benchmarking purposes.
pub fn compute_template_stats(template: &GrayImage, mask: Option<&GrayImage>) -> TemplateStats {
    let mut sum = 0.0;
    let mut sum_sq = 0.0;
    let mut count = 0usize;

    for (x, y, pixel) in template.enumerate_pixels() {
        let use_pixel = mask
            .map(|m| m.get_pixel(x.min(m.width() - 1), y.min(m.height() - 1)).0[0] > 128)
            .unwrap_or(true);

        if use_pixel {
            let val = pixel.0[0] as f64;
            sum += val;
            sum_sq += val * val;
            count += 1;
        }
    }

    if count == 0 {
        return TemplateStats {
            mean: 0.0,
            std: 0.0,
            pixel_count: 0,
        };
    }

    let mean = sum / count as f64;
    let variance = (sum_sq / count as f64) - (mean * mean);
    let std = variance.max(0.0).sqrt();

    TemplateStats {
        mean,
        std,
        pixel_count: count,
    }
}

/// Compute NCC score at a specific position (scalar version).
#[allow(clippy::too_many_arguments)]
fn compute_ncc_at(
    image: &GrayImage,
    template: &GrayImage,
    mask: Option<&GrayImage>,
    offset_x: u32,
    offset_y: u32,
    tpl_stats: &TemplateStats,
) -> f64 {
    let tpl_w = template.width();
    let tpl_h = template.height();

    // Compute image region statistics
    let mut img_sum = 0.0;
    let mut img_sum_sq = 0.0;
    let mut cross_sum = 0.0;

    for ty in 0..tpl_h {
        for tx in 0..tpl_w {
            let use_pixel = mask
                .map(|m| m.get_pixel(tx.min(m.width() - 1), ty.min(m.height() - 1)).0[0] > 128)
                .unwrap_or(true);

            if use_pixel {
                let img_val = image.get_pixel(offset_x + tx, offset_y + ty).0[0] as f64;
                let tpl_val = template.get_pixel(tx, ty).0[0] as f64;

                img_sum += img_val;
                img_sum_sq += img_val * img_val;
                cross_sum += img_val * tpl_val;
            }
        }
    }

    let count = tpl_stats.pixel_count as f64;
    let img_mean = img_sum / count;
    let img_variance = (img_sum_sq / count) - (img_mean * img_mean);
    let img_std = img_variance.max(0.0).sqrt();

    if img_std < f64::EPSILON {
        return 0.0;
    }

    // NCC = sum((I - mean_I) * (T - mean_T)) / (n * std_I * std_T)
    // Expanded: (sum(I*T) - n*mean_I*mean_T) / (n * std_I * std_T)
    let numerator = cross_sum - count * img_mean * tpl_stats.mean;
    let denominator = count * img_std * tpl_stats.std;

    if denominator < f64::EPSILON {
        return 0.0;
    }

    (numerator / denominator).clamp(-1.0, 1.0)
}

/// SIMD-accelerated NCC computation using the `wide` crate.
///
/// This version processes 8 pixels at a time using f32x8 SIMD vectors.
/// Only used when:
/// - The `find_image_simd` feature is enabled
/// - No mask is present (masks require per-pixel conditional logic)
/// - Template width >= 16 (to amortize SIMD overhead)
#[cfg(feature = "find_image_simd")]
#[allow(clippy::too_many_arguments)]
fn compute_ncc_at_simd(
    image: &GrayImage,
    template: &GrayImage,
    offset_x: u32,
    offset_y: u32,
    tpl_stats: &TemplateStats,
) -> f64 {
    use wide::f32x8;

    let tpl_w = template.width() as usize;
    let tpl_h = template.height() as usize;
    let img_stride = image.width() as usize;

    let mut img_sum_acc = f32x8::ZERO;
    let mut img_sum_sq_acc = f32x8::ZERO;
    let mut cross_sum_acc = f32x8::ZERO;

    // Scalar accumulators for remainder
    let mut img_sum_scalar = 0.0f32;
    let mut img_sum_sq_scalar = 0.0f32;
    let mut cross_sum_scalar = 0.0f32;

    let image_raw = image.as_raw();
    let template_raw = template.as_raw();

    for ty in 0..tpl_h {
        let img_row_start = (offset_y as usize + ty) * img_stride + offset_x as usize;
        let tpl_row_start = ty * tpl_w;

        let mut tx = 0usize;

        // Process 8 pixels at a time
        while tx + 8 <= tpl_w {
            // Load 8 image pixels
            let img_slice = &image_raw[img_row_start + tx..img_row_start + tx + 8];
            let img_vals = f32x8::new([
                img_slice[0] as f32,
                img_slice[1] as f32,
                img_slice[2] as f32,
                img_slice[3] as f32,
                img_slice[4] as f32,
                img_slice[5] as f32,
                img_slice[6] as f32,
                img_slice[7] as f32,
            ]);

            // Load 8 template pixels
            let tpl_slice = &template_raw[tpl_row_start + tx..tpl_row_start + tx + 8];
            let tpl_vals = f32x8::new([
                tpl_slice[0] as f32,
                tpl_slice[1] as f32,
                tpl_slice[2] as f32,
                tpl_slice[3] as f32,
                tpl_slice[4] as f32,
                tpl_slice[5] as f32,
                tpl_slice[6] as f32,
                tpl_slice[7] as f32,
            ]);

            img_sum_acc += img_vals;
            img_sum_sq_acc += img_vals * img_vals;
            cross_sum_acc += img_vals * tpl_vals;

            tx += 8;
        }

        // Handle remaining pixels (scalar)
        while tx < tpl_w {
            let img_val = image_raw[img_row_start + tx] as f32;
            let tpl_val = template_raw[tpl_row_start + tx] as f32;

            img_sum_scalar += img_val;
            img_sum_sq_scalar += img_val * img_val;
            cross_sum_scalar += img_val * tpl_val;

            tx += 1;
        }
    }

    // Reduce SIMD accumulators
    let img_sum_arr: [f32; 8] = img_sum_acc.into();
    let img_sum_sq_arr: [f32; 8] = img_sum_sq_acc.into();
    let cross_sum_arr: [f32; 8] = cross_sum_acc.into();

    let img_sum: f64 = img_sum_arr.iter().map(|&x| x as f64).sum::<f64>() + img_sum_scalar as f64;
    let img_sum_sq: f64 =
        img_sum_sq_arr.iter().map(|&x| x as f64).sum::<f64>() + img_sum_sq_scalar as f64;
    let cross_sum: f64 =
        cross_sum_arr.iter().map(|&x| x as f64).sum::<f64>() + cross_sum_scalar as f64;

    // Compute NCC
    let count = tpl_stats.pixel_count as f64;
    let img_mean = img_sum / count;
    let img_variance = (img_sum_sq / count) - (img_mean * img_mean);
    let img_std = img_variance.max(0.0).sqrt();

    if img_std < f64::EPSILON {
        return 0.0;
    }

    let numerator = cross_sum - count * img_mean * tpl_stats.mean;
    let denominator = count * img_std * tpl_stats.std;

    if denominator < f64::EPSILON {
        return 0.0;
    }

    (numerator / denominator).clamp(-1.0, 1.0)
}

/// Non-Maximum Suppression to remove overlapping detections.
fn non_maximum_suppression(
    mut matches: Vec<MatchResult>,
    iou_threshold: f64,
    max_results: usize,
) -> Vec<MatchResult> {
    // Sort by score descending
    matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut keep = Vec::new();

    while !matches.is_empty() && keep.len() < max_results {
        let best = matches.remove(0);

        // Remove all matches that overlap too much with the best
        matches.retain(|m| compute_iou(&best.bbox, &m.bbox) < iou_threshold);

        keep.push(best);
    }

    keep
}

/// Compute Intersection over Union of two bounding boxes.
fn compute_iou(a: &BoundingBox, b: &BoundingBox) -> f64 {
    let x1 = a.x.max(b.x);
    let y1 = a.y.max(b.y);
    let x2 = (a.x + a.w).min(b.x + b.w);
    let y2 = (a.y + a.h).min(b.y + b.h);

    if x2 <= x1 || y2 <= y1 {
        return 0.0;
    }

    let intersection = (x2 - x1) as f64 * (y2 - y1) as f64;
    let area_a = a.w as f64 * a.h as f64;
    let area_b = b.w as f64 * b.h as f64;
    let union = area_a + area_b - intersection;

    if union < f64::EPSILON {
        return 0.0;
    }

    intersection / union
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn test_nms() {
        let matches = vec![
            MatchResult {
                score: 0.9,
                bbox: BoundingBox {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                },
                center: Point { x: 5.0, y: 5.0 },
                scale: 1.0,
                rotation: 0.0,
                screen_x: None,
                screen_y: None,
            },
            MatchResult {
                score: 0.85,
                bbox: BoundingBox {
                    x: 2,
                    y: 2,
                    w: 10,
                    h: 10,
                },
                center: Point { x: 7.0, y: 7.0 },
                scale: 1.0,
                rotation: 0.0,
                screen_x: None,
                screen_y: None,
            },
            MatchResult {
                score: 0.8,
                bbox: BoundingBox {
                    x: 50,
                    y: 50,
                    w: 10,
                    h: 10,
                },
                center: Point { x: 55.0, y: 55.0 },
                scale: 1.0,
                rotation: 0.0,
                screen_x: None,
                screen_y: None,
            },
        ];

        let result = non_maximum_suppression(matches, 0.3, 5);
        // First two overlap significantly, third doesn't
        assert_eq!(result.len(), 2);
        assert!((result[0].score - 0.9).abs() < f64::EPSILON);
        assert!((result[1].score - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ncc_matching_finds_exact_template() {
        // Create a 100x100 image with a distinct gradient pattern at (30, 40)
        let mut image = GrayImage::from_fn(100, 100, |_, _| Luma([128]));

        // Draw a gradient pattern at position (30, 40) - needs variation for NCC
        for y in 0..10u32 {
            for x in 0..10u32 {
                // Create a diagonal gradient pattern
                let val = ((x + y) * 12 + 50) as u8;
                image.put_pixel(30 + x, 40 + y, Luma([val]));
            }
        }

        // Create template that matches the pattern exactly
        let template = GrayImage::from_fn(10, 10, |x, y| {
            let val = ((x + y) * 12 + 50) as u8;
            Luma([val])
        });

        // Run NCC matching
        let matches = match_template_ncc(&image, &template, None, 0.9, 1);

        // Should find the match
        assert!(!matches.is_empty(), "Should find at least one match");

        // Best match should be near (30, 40)
        let (best_x, best_y, best_score) = matches
            .iter()
            .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap())
            .cloned()
            .unwrap();

        assert!(
            best_score > 0.95,
            "Score should be very high for exact match, got {}",
            best_score
        );
        assert_eq!(best_x, 30, "X should be 30, got {}", best_x);
        assert_eq!(best_y, 40, "Y should be 40, got {}", best_y);
    }

    #[test]
    fn test_ncc_no_match_for_different_pattern() {
        // Create image with a horizontal gradient
        let image = GrayImage::from_fn(50, 50, |x, _| Luma([(x * 5) as u8]));

        // Template with vertical gradient (orthogonal pattern)
        let template = GrayImage::from_fn(10, 10, |_, y| Luma([(y * 25) as u8]));

        // High threshold, should find no good matches for orthogonal patterns
        let matches = match_template_ncc(&image, &template, None, 0.9, 1);

        // NCC can produce negative correlation for orthogonal patterns
        // so we check that high-threshold matches are not found
        assert!(
            matches.is_empty(),
            "Should not find match for orthogonal pattern at high threshold, got {} matches",
            matches.len()
        );
    }

    #[test]
    fn test_extract_region_clamps_to_bounds() {
        let img = GrayImage::from_fn(100, 100, |x, y| Luma([(x + y) as u8]));

        // Region exceeds image bounds
        let region = SearchRegion {
            x: 80,
            y: 90,
            w: 50,
            h: 50,
        };
        let extracted = extract_region(&img, &region);

        // Should be clamped to available space
        assert_eq!(extracted.width(), 20); // 100 - 80
        assert_eq!(extracted.height(), 10); // 100 - 90
    }

    #[test]
    fn test_rotate_image_90_degrees() {
        // Create a simple 2x3 image
        let img = GrayImage::from_vec(2, 3, vec![1, 2, 3, 4, 5, 6]).unwrap();
        let rotated = rotate_image(&img, 90.0);

        // After 90 degree rotation, 2x3 becomes 3x2
        assert_eq!(rotated.width(), 3);
        assert_eq!(rotated.height(), 2);
    }

    #[test]
    fn test_resize_image_zero_scale() {
        let img = GrayImage::from_fn(10, 10, |_, _| Luma([128]));

        // Very small scale should produce 1x1 minimum
        let tiny = resize_image(&img, 0.01);
        assert!(tiny.width() >= 1);
        assert!(tiny.height() >= 1);
    }

    // Tests now use the production normalize_rotation function from super::*

    #[test]
    fn test_rotation_normalization_wrapping() {
        // Values that wrap around 360
        assert_eq!(normalize_rotation(360.0), Some(0.0));
        assert_eq!(normalize_rotation(450.0), Some(90.0));
        assert_eq!(normalize_rotation(540.0), Some(180.0));
        assert_eq!(normalize_rotation(-90.0), Some(270.0));
        assert_eq!(normalize_rotation(-270.0), Some(90.0));
    }

    #[test]
    fn test_scale_validation_step_must_be_positive() {
        // Zero step
        let result = validate_scale_range(&ScaleRange {
            min: 0.8,
            max: 1.2,
            step: 0.0,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("step"));

        // Negative step
        let result = validate_scale_range(&ScaleRange {
            min: 0.8,
            max: 1.2,
            step: -0.1,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("step"));
    }

    #[test]
    fn test_scale_validation_min_must_be_positive() {
        // Zero min
        let result = validate_scale_range(&ScaleRange {
            min: 0.0,
            max: 1.2,
            step: 0.1,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("min"));

        // Negative min
        let result = validate_scale_range(&ScaleRange {
            min: -0.5,
            max: 1.2,
            step: 0.1,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("min"));
    }

    #[test]
    fn test_scale_validation_max_must_be_positive() {
        // Zero max
        let result = validate_scale_range(&ScaleRange {
            min: 0.5,
            max: 0.0,
            step: 0.1,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max"));

        // Negative max
        let result = validate_scale_range(&ScaleRange {
            min: 0.5,
            max: -1.0,
            step: 0.1,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max"));
    }

    #[test]
    fn test_scale_validation_min_not_exceed_max() {
        let result = validate_scale_range(&ScaleRange {
            min: 2.0,
            max: 0.5,
            step: 0.1,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not exceed"));
    }

    // ============ Integration tests for template_id / mask_id ============

    use crate::tools::image_cache::{ImageCache, ImageMetadata};
    use image::GenericImage;
    use std::io::Cursor;

    /// Create a test PNG in memory and return bytes
    fn create_test_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(width, height);
        let mut cursor = Cursor::new(Vec::new());
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        cursor.into_inner()
    }

    fn make_image_metadata(width: u32, height: u32) -> ImageMetadata {
        ImageMetadata {
            source_path: None,
            width,
            height,
            channels: 3,
            mime: "image/png".to_string(),
            sha256: None,
            is_mask: false,
        }
    }

    #[tokio::test]
    async fn test_find_image_missing_template_id_errors() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(100, 100)),
            ),
            template_id: Some("nonexistent-id".to_string()),
            template_image_base64: None,
            mask_id: None,
            mask_image_base64: None,
            mode: "fast".to_string(),
            threshold: None,
            max_results: None,
            scales: None,
            rotations: None,
            search_region: None,
            stride: None,
            return_screen_coords: false,
        };

        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(result.is_error.unwrap_or(false));
        // Check error message mentions template ID
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            assert!(text.contains("Template ID") && text.contains("not found"));
        }
    }

    #[tokio::test]
    async fn test_find_image_missing_mask_id_errors() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        // Add a template to the cache
        {
            let mut cache = image_cache.write().await;
            cache.store(
                create_test_png_bytes(32, 32),
                make_image_metadata(32, 32),
                Some("template"),
            );
        }

        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(100, 100)),
            ),
            template_id: None,
            template_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(16, 16)),
            ),
            mask_id: Some("nonexistent-mask".to_string()),
            mask_image_base64: None,
            mode: "fast".to_string(),
            threshold: None,
            max_results: None,
            scales: None,
            rotations: None,
            search_region: None,
            stride: None,
            return_screen_coords: false,
        };

        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(result.is_error.unwrap_or(false));
        // Check error message mentions mask ID
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            assert!(text.contains("Mask ID") && text.contains("not found"));
        }
    }

    #[tokio::test]
    async fn test_find_image_requires_template_source() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(100, 100)),
            ),
            template_id: None,
            template_image_base64: None, // Neither template_id nor template_image_base64
            mask_id: None,
            mask_image_base64: None,
            mode: "fast".to_string(),
            threshold: None,
            max_results: None,
            scales: None,
            rotations: None,
            search_region: None,
            stride: None,
            return_screen_coords: false,
        };

        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(result.is_error.unwrap_or(false));
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            assert!(text.contains("template_id") || text.contains("template_image_base64"));
        }
    }

    #[tokio::test]
    async fn test_find_image_with_template_id_from_cache() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        // Create a pattern with variation (gradient) that NCC can match
        let mut screenshot_img = image::DynamicImage::new_rgb8(100, 100);
        // Fill with gray background
        for y in 0..100u32 {
            for x in 0..100u32 {
                screenshot_img.put_pixel(x, y, image::Rgba([128, 128, 128, 255]));
            }
        }
        // Draw a diagonal gradient pattern at (30, 40)
        for y in 0..10u32 {
            for x in 0..10u32 {
                let val = ((x + y) * 12 + 50) as u8;
                screenshot_img.put_pixel(30 + x, 40 + y, image::Rgba([val, val, val, 255]));
            }
        }
        let mut screenshot_bytes = Cursor::new(Vec::new());
        screenshot_img
            .write_to(&mut screenshot_bytes, image::ImageFormat::Png)
            .unwrap();

        // Create template matching the gradient pattern
        let mut template_img = image::DynamicImage::new_rgb8(10, 10);
        for y in 0..10u32 {
            for x in 0..10u32 {
                let val = ((x + y) * 12 + 50) as u8;
                template_img.put_pixel(x, y, image::Rgba([val, val, val, 255]));
            }
        }
        let mut template_bytes = Cursor::new(Vec::new());
        template_img
            .write_to(&mut template_bytes, image::ImageFormat::Png)
            .unwrap();

        // Add template to cache
        let template_id = {
            let mut cache = image_cache.write().await;
            cache.store(
                template_bytes.into_inner(),
                make_image_metadata(10, 10),
                Some("template"),
            )
        };

        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(screenshot_bytes.into_inner()),
            ),
            template_id: Some(template_id),
            template_image_base64: None,
            mask_id: None,
            mask_image_base64: None,
            mode: "fast".to_string(),
            threshold: Some(0.9),
            max_results: None,
            scales: Some(ScaleRange {
                min: 1.0,
                max: 1.0,
                step: 0.1,
            }), // Exact scale only
            rotations: None,
            search_region: None,
            stride: Some(1),
            return_screen_coords: false,
        };

        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(
            !result.is_error.unwrap_or(true),
            "find_image should succeed"
        );

        // Parse response and verify we got a match
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            let response: FindImageResponse = serde_json::from_str(text).unwrap();
            assert!(
                !response.matches.is_empty(),
                "Should find at least one match"
            );
            // The match should be near (30, 40)
            let best = &response.matches[0];
            assert!(
                best.bbox.x >= 28 && best.bbox.x <= 32,
                "x should be near 30, got {}",
                best.bbox.x
            );
            assert!(
                best.bbox.y >= 38 && best.bbox.y <= 42,
                "y should be near 40, got {}",
                best.bbox.y
            );
        }
    }

    #[tokio::test]
    async fn test_find_image_stale_template_id_falls_back_to_base64() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        // Provide a stale template_id but valid base64 fallback
        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(50, 50)),
            ),
            template_id: Some("stale-id-not-in-cache".to_string()),
            template_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(8, 8)),
            ),
            mask_id: None,
            mask_image_base64: None,
            mode: "fast".to_string(),
            threshold: Some(0.5),
            max_results: None,
            scales: Some(ScaleRange {
                min: 1.0,
                max: 1.0,
                step: 0.1,
            }),
            rotations: None,
            search_region: None,
            stride: None,
            return_screen_coords: false,
        };

        // Should succeed using base64 fallback with a warning
        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(
            !result.is_error.unwrap_or(true),
            "Should succeed using base64 fallback"
        );

        // Verify warning is present
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            let response: FindImageResponse = serde_json::from_str(text).unwrap();
            assert!(
                response.warning.is_some(),
                "Should have warning about stale ID"
            );
            assert!(
                response
                    .warning
                    .as_ref()
                    .unwrap()
                    .contains("not found in cache"),
                "Warning should mention cache miss"
            );
        }
    }

    #[tokio::test]
    async fn test_find_image_stale_mask_id_falls_back_to_base64() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        // Provide valid template but stale mask_id with base64 fallback
        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(50, 50)),
            ),
            template_id: None,
            template_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(8, 8)),
            ),
            mask_id: Some("stale-mask-id".to_string()),
            mask_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(8, 8)),
            ),
            mode: "fast".to_string(),
            threshold: Some(0.5),
            max_results: None,
            scales: Some(ScaleRange {
                min: 1.0,
                max: 1.0,
                step: 0.1,
            }),
            rotations: None,
            search_region: None,
            stride: None,
            return_screen_coords: false,
        };

        // Should succeed using base64 fallback
        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(
            !result.is_error.unwrap_or(true),
            "Should succeed using mask base64 fallback"
        );

        // Verify warning is present
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            let response: FindImageResponse = serde_json::from_str(text).unwrap();
            assert!(
                response.warning.is_some(),
                "Should have warning about stale mask ID"
            );
            assert!(
                response.warning.as_ref().unwrap().contains("Mask ID"),
                "Warning should mention mask"
            );
        }
    }

    #[tokio::test]
    async fn test_find_image_mask_dimension_mismatch_errors() {
        let screenshot_cache = Arc::new(RwLock::new(ScreenshotCache::default()));
        let image_cache = Arc::new(RwLock::new(ImageCache::default()));

        // Template is 10x10, mask is 8x8 - dimension mismatch
        let params = FindImageParams {
            screenshot_id: None,
            screenshot_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(50, 50)),
            ),
            template_id: None,
            template_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(10, 10)),
            ),
            mask_id: None,
            mask_image_base64: Some(
                base64::engine::general_purpose::STANDARD.encode(create_test_png_bytes(8, 8)),
            ),
            mode: "fast".to_string(),
            threshold: None,
            max_results: None,
            scales: Some(ScaleRange {
                min: 1.0,
                max: 1.0,
                step: 0.1,
            }),
            rotations: None,
            search_region: None,
            stride: None,
            return_screen_coords: false,
        };

        let result = find_image(params, screenshot_cache, image_cache).await;
        assert!(
            result.is_error.unwrap_or(false),
            "Should error on mask dimension mismatch"
        );

        // Verify error message mentions dimension mismatch
        if let rmcp::model::ContentBlock::Text(rmcp::model::TextContent { text, .. }) =
            &result.content[0]
        {
            assert!(
                text.contains("Mask dimensions") && text.contains("must match"),
                "Error should mention mask dimension mismatch, got: {}",
                text
            );
        }
    }

    // ============ Tests for optimization functions ============

    #[test]
    fn test_compute_downscale_factor_small_image() {
        // Image smaller than 1200px max dimension - no downscale
        let img = GrayImage::new(800, 600);
        let template = GrayImage::new(32, 32);
        let factor = compute_downscale_factor(&img, &template);
        assert!(
            (factor - 1.0).abs() < f64::EPSILON,
            "Small images should not be downscaled"
        );
    }

    #[test]
    fn test_compute_downscale_factor_large_image() {
        // 1920x1080 image - should be downscaled
        let img = GrayImage::new(1920, 1080);
        let template = GrayImage::new(32, 32);
        let factor = compute_downscale_factor(&img, &template);
        // 1200 / 1920 = 0.625
        assert!(factor < 1.0, "Large images should be downscaled");
        assert!(factor >= 0.5, "Downscale should not go below 0.5");
        assert!(
            (factor - 0.625).abs() < 0.01,
            "Expected ~0.625, got {}",
            factor
        );
    }

    #[test]
    fn test_compute_downscale_factor_very_large_image() {
        // 4K image - should be capped at 0.5
        let img = GrayImage::new(3840, 2160);
        let template = GrayImage::new(32, 32);
        let factor = compute_downscale_factor(&img, &template);
        // 1200 / 3840 = 0.3125, but capped at 0.5
        assert!(
            (factor - 0.5).abs() < f64::EPSILON,
            "Very large images should cap at 0.5"
        );
    }

    #[test]
    fn test_build_work_items() {
        let rotations = vec![0.0, 90.0];
        let template = GrayImage::new(10, 10);
        let search_img = GrayImage::new(100, 100);
        let rotated_templates = build_rotated_templates(&template, None, &rotations);
        let scales = ScaleRange {
            min: 0.8,
            max: 1.2,
            step: 0.2,
        };
        let items = build_work_items(&rotations, &scales, &rotated_templates, &search_img);

        // Should have: 0.8, 1.0, 1.2 for each rotation = 3 * 2 = 6 items
        assert_eq!(items.len(), 6, "Expected 6 work items, got {}", items.len());

        // Verify first rotation (0.0) items
        assert!((items[0].rotation - 0.0).abs() < f64::EPSILON);
        assert!((items[0].scale - 0.8).abs() < f64::EPSILON);
        assert!((items[1].rotation - 0.0).abs() < f64::EPSILON);
        assert!((items[1].scale - 1.0).abs() < f64::EPSILON);
        assert!((items[2].rotation - 0.0).abs() < f64::EPSILON);
        assert!((items[2].scale - 1.2).abs() < f64::EPSILON);

        // Verify second rotation (90.0) items
        assert!((items[3].rotation - 90.0).abs() < f64::EPSILON);
        assert!((items[3].scale - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_build_work_items_single_scale() {
        let rotations = vec![0.0];
        let template = GrayImage::new(10, 10);
        let search_img = GrayImage::new(100, 100);
        let rotated_templates = build_rotated_templates(&template, None, &rotations);
        let scales = ScaleRange {
            min: 1.0,
            max: 1.0,
            step: 0.1,
        };
        let items = build_work_items(&rotations, &scales, &rotated_templates, &search_img);

        assert_eq!(items.len(), 1, "Expected 1 work item for single scale");
        assert!((items[0].scale - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_process_work_item_template_too_large() {
        let search_img = GrayImage::new(50, 50);
        let template = GrayImage::new(100, 100); // Larger than search image

        let item = WorkItem {
            rotation: 0.0,
            rotation_idx: 0,
            scale: 1.0,
        };

        let result = process_work_item(
            &item,
            &search_img,
            &template,
            None,
            0.88,
            1,
            (0, 0),
            1.0,
            None,
            false,
        );

        assert!(
            result.is_none(),
            "Should return None when template exceeds image"
        );
    }

    #[test]
    fn test_process_work_item_downscale_coordinate_mapping() {
        // Create a search image and template
        let mut search_img = GrayImage::from_fn(100, 100, |_, _| Luma([128]));

        // Place a distinct pattern at position (40, 50)
        for y in 0..10u32 {
            for x in 0..10u32 {
                let val = ((x + y) * 12 + 50) as u8;
                search_img.put_pixel(40 + x, 50 + y, Luma([val]));
            }
        }

        // Create matching template
        let template = GrayImage::from_fn(10, 10, |x, y| {
            let val = ((x + y) * 12 + 50) as u8;
            Luma([val])
        });

        let item = WorkItem {
            rotation: 0.0,
            rotation_idx: 0,
            scale: 1.0,
        };

        // Test with downscale_factor = 1.0 (no downscaling)
        let result = process_work_item(
            &item,
            &search_img,
            &template,
            None,
            0.9,
            1,
            (0, 0),
            1.0, // No downscale
            None,
            false,
        );

        assert!(result.is_some(), "Should find matches");
        let matches = result.unwrap();
        assert!(!matches.is_empty(), "Should have at least one match");

        // Best match should be near (40, 50)
        let best = matches
            .iter()
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
            .unwrap();
        assert!(
            (best.bbox.x as i32 - 40).abs() <= 2,
            "X should be near 40, got {}",
            best.bbox.x
        );
        assert!(
            (best.bbox.y as i32 - 50).abs() <= 2,
            "Y should be near 50, got {}",
            best.bbox.y
        );
    }

    #[cfg(feature = "find_image_simd")]
    #[test]
    fn test_simd_ncc_matches_scalar() {
        // Create test images
        let image = GrayImage::from_fn(100, 100, |x, y| {
            Luma([((x.wrapping_mul(3).wrapping_add(y.wrapping_mul(7))) % 255) as u8])
        });
        let template = GrayImage::from_fn(20, 20, |x, y| {
            Luma([((x.wrapping_mul(3).wrapping_add(y.wrapping_mul(7))) % 255) as u8])
        });

        let tpl_stats = compute_template_stats(&template, None);

        // Compute scalar result
        let scalar_score = compute_ncc_at(&image, &template, None, 0, 0, &tpl_stats);

        // Compute SIMD result
        let simd_score = compute_ncc_at_simd(&image, &template, 0, 0, &tpl_stats);

        // Results should be very close (within floating point tolerance)
        assert!(
            (scalar_score - simd_score).abs() < 0.001,
            "SIMD and scalar should match: scalar={}, simd={}",
            scalar_score,
            simd_score
        );
    }
}
