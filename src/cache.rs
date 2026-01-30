//! Modal image cache management.
//!
//! This module provides functionality to cache Modal image IDs to avoid rebuilding
//! images that haven't changed. The cache is stored in `{working_dir}/.offload/modal_images.json`.
//!
//! # Cache Structure
//!
//! The cache maps cache keys (like "default", "rust", or "dockerfile:/path/to/Dockerfile")
//! to image metadata including the Modal image ID and optional dockerfile hash.
//!
//! # Example
//!
//! ```no_run
//! use offload::cache::{ImageCache, ImageCacheEntry};
//! use std::path::Path;
//!
//! let cache_dir = Path::new("/path/to/project");
//! let mut cache = ImageCache::load(cache_dir);
//!
//! // Check if we have a cached image
//! if let Some(entry) = cache.get("default") {
//!     println!("Found cached image: {}", entry.image_id);
//! }
//!
//! // Add a new cache entry
//! cache.insert("default".to_string(), ImageCacheEntry {
//!     image_id: "im-abc123".to_string(),
//!     dockerfile_hash: None,
//!     created_at: chrono::Utc::now().to_rfc3339(),
//!     image_type: "preset".to_string(),
//! });
//!
//! // Save the cache
//! cache.save(cache_dir).unwrap();
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

/// A cache entry for a Modal image.
///
/// Contains metadata about a cached Modal image, including its ID and
/// optional dockerfile hash for validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageCacheEntry {
    /// The Modal image ID (e.g., "im-abc123")
    pub image_id: String,

    /// SHA256 hash of the dockerfile contents (if applicable)
    pub dockerfile_hash: Option<String>,

    /// ISO 8601 timestamp when the entry was created
    pub created_at: String,

    /// Type of image: "dockerfile" or "preset" (e.g., "default", "rust")
    pub image_type: String,
}

/// Cache for Modal image IDs.
///
/// Manages a persistent cache of Modal image IDs to avoid rebuilding images
/// that haven't changed. The cache is stored as JSON in `.offload/modal_images.json`.
#[derive(Debug, Clone, Default)]
pub struct ImageCache {
    entries: HashMap<String, ImageCacheEntry>,
}

impl ImageCache {
    /// Loads the cache from disk.
    ///
    /// Reads from `{cache_dir}/.offload/modal_images.json`. If the file doesn't exist
    /// or is invalid, returns an empty cache.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - The directory containing the `.offload` cache directory
    ///
    /// # Example
    ///
    /// ```no_run
    /// use offload::cache::ImageCache;
    /// use std::path::Path;
    ///
    /// let cache = ImageCache::load(Path::new("/path/to/project"));
    /// ```
    pub fn load(cache_dir: &Path) -> Self {
        let cache_path = cache_dir.join(".offload").join("modal_images.json");

        tracing::debug!("Loading image cache from: {}", cache_path.display());

        if !cache_path.exists() {
            tracing::debug!("Cache file does not exist, returning empty cache");
            return Self::default();
        }

        match fs::read_to_string(&cache_path) {
            Ok(contents) => {
                match serde_json::from_str::<HashMap<String, ImageCacheEntry>>(&contents) {
                    Ok(entries) => {
                        tracing::debug!("Loaded {} cache entries", entries.len());
                        Self { entries }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse cache file, returning empty cache: {}", e);
                        Self::default()
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to read cache file, returning empty cache: {}", e);
                Self::default()
            }
        }
    }

    /// Saves the cache to disk.
    ///
    /// Writes to `{cache_dir}/.offload/modal_images.json`. Creates the `.offload`
    /// directory if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - The directory containing the `.offload` cache directory
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file cannot be written.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use offload::cache::ImageCache;
    /// use std::path::Path;
    ///
    /// let cache = ImageCache::load(Path::new("/path/to/project"));
    /// cache.save(Path::new("/path/to/project")).unwrap();
    /// ```
    pub fn save(&self, cache_dir: &Path) -> Result<()> {
        let offload_dir = cache_dir.join(".offload");
        let cache_path = offload_dir.join("modal_images.json");

        tracing::debug!("Saving image cache to: {}", cache_path.display());

        // Create .offload directory if it doesn't exist
        fs::create_dir_all(&offload_dir).context("Failed to create .offload directory")?;

        // Serialize and write cache
        let contents =
            serde_json::to_string_pretty(&self.entries).context("Failed to serialize cache")?;

        fs::write(&cache_path, contents).context("Failed to write cache file")?;

        tracing::debug!("Saved {} cache entries", self.entries.len());

        Ok(())
    }

    /// Gets a cache entry by key.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key (e.g., "default", "rust", "dockerfile:/path/to/Dockerfile")
    ///
    /// # Returns
    ///
    /// The cache entry if found, or `None` if the key doesn't exist.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use offload::cache::ImageCache;
    /// use std::path::Path;
    ///
    /// let cache = ImageCache::load(Path::new("/path/to/project"));
    /// if let Some(entry) = cache.get("default") {
    ///     println!("Found cached image: {}", entry.image_id);
    /// }
    /// ```
    pub fn get(&self, key: &str) -> Option<&ImageCacheEntry> {
        self.entries.get(key)
    }

    /// Gets a cache entry for a dockerfile, validating the hash.
    ///
    /// Returns the entry only if it exists and the dockerfile hash matches the current hash.
    /// This ensures we don't use a stale cache entry when the dockerfile has changed.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key (e.g., "dockerfile:/path/to/Dockerfile")
    /// * `current_hash` - Current SHA256 hash of the dockerfile
    ///
    /// # Returns
    ///
    /// The cache entry if found and the hash matches, or `None` otherwise.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use offload::cache::{ImageCache, compute_file_hash};
    /// use std::path::Path;
    ///
    /// let cache = ImageCache::load(Path::new("/path/to/project"));
    /// let dockerfile_path = Path::new("Dockerfile");
    /// let current_hash = compute_file_hash(dockerfile_path).unwrap();
    ///
    /// if let Some(entry) = cache.get_for_dockerfile("dockerfile:Dockerfile", &current_hash) {
    ///     println!("Found valid cached image: {}", entry.image_id);
    /// }
    /// ```
    pub fn get_for_dockerfile(&self, key: &str, current_hash: &str) -> Option<&ImageCacheEntry> {
        let entry = self.entries.get(key)?;

        // Validate hash matches
        match &entry.dockerfile_hash {
            Some(cached_hash) if cached_hash == current_hash => Some(entry),
            Some(cached_hash) => {
                tracing::debug!(
                    "Dockerfile hash mismatch for key '{}': cached={}, current={}",
                    key,
                    cached_hash,
                    current_hash
                );
                None
            }
            None => {
                tracing::debug!("No dockerfile hash in cache entry for key '{}'", key);
                None
            }
        }
    }

    /// Inserts or updates a cache entry.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key (e.g., "default", "rust", "dockerfile:/path/to/Dockerfile")
    /// * `entry` - The cache entry to insert
    ///
    /// # Example
    ///
    /// ```no_run
    /// use offload::cache::{ImageCache, ImageCacheEntry};
    /// use std::path::Path;
    ///
    /// let mut cache = ImageCache::load(Path::new("/path/to/project"));
    /// cache.insert("default".to_string(), ImageCacheEntry {
    ///     image_id: "im-abc123".to_string(),
    ///     dockerfile_hash: None,
    ///     created_at: chrono::Utc::now().to_rfc3339(),
    ///     image_type: "preset".to_string(),
    /// });
    /// ```
    pub fn insert(&mut self, key: String, entry: ImageCacheEntry) {
        tracing::debug!("Inserting cache entry for key '{}': {:?}", key, entry);
        self.entries.insert(key, entry);
    }
}

/// Computes the SHA256 hash of a file.
///
/// Reads the file in chunks to avoid loading large files entirely into memory.
///
/// # Arguments
///
/// * `path` - Path to the file to hash
///
/// # Returns
///
/// The SHA256 hash as a lowercase hexadecimal string.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
///
/// # Example
///
/// ```no_run
/// use offload::cache::compute_file_hash;
/// use std::path::Path;
///
/// let hash = compute_file_hash(Path::new("Dockerfile")).unwrap();
/// println!("Dockerfile hash: {}", hash);
/// ```
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).context(format!("Failed to open file: {}", path.display()))?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .context(format!("Failed to read file: {}", path.display()))?;

        if bytes_read == 0 {
            break;
        }

        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    Ok(format!("{:x}", result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_empty_cache_load() {
        let temp_dir = TempDir::new().unwrap();
        let cache = ImageCache::load(temp_dir.path());

        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_cache_save_and_load() {
        let temp_dir = TempDir::new().unwrap();

        // Create and save a cache
        let mut cache = ImageCache::default();
        cache.insert(
            "default".to_string(),
            ImageCacheEntry {
                image_id: "im-abc123".to_string(),
                dockerfile_hash: None,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                image_type: "preset".to_string(),
            },
        );

        cache.save(temp_dir.path()).unwrap();

        // Load the cache and verify
        let loaded_cache = ImageCache::load(temp_dir.path());
        assert_eq!(loaded_cache.entries.len(), 1);

        let entry = loaded_cache.get("default").unwrap();
        assert_eq!(entry.image_id, "im-abc123");
        assert_eq!(entry.image_type, "preset");
        assert!(entry.dockerfile_hash.is_none());
    }

    #[test]
    fn test_cache_get() {
        let mut cache = ImageCache::default();
        cache.insert(
            "default".to_string(),
            ImageCacheEntry {
                image_id: "im-abc123".to_string(),
                dockerfile_hash: None,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                image_type: "preset".to_string(),
            },
        );

        assert!(cache.get("default").is_some());
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn test_cache_get_for_dockerfile_matching_hash() {
        let mut cache = ImageCache::default();
        let hash = "abc123";

        cache.insert(
            "dockerfile:test".to_string(),
            ImageCacheEntry {
                image_id: "im-def456".to_string(),
                dockerfile_hash: Some(hash.to_string()),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                image_type: "dockerfile".to_string(),
            },
        );

        // Matching hash should return the entry
        assert!(cache.get_for_dockerfile("dockerfile:test", hash).is_some());
    }

    #[test]
    fn test_cache_get_for_dockerfile_mismatched_hash() {
        let mut cache = ImageCache::default();

        cache.insert(
            "dockerfile:test".to_string(),
            ImageCacheEntry {
                image_id: "im-def456".to_string(),
                dockerfile_hash: Some("abc123".to_string()),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                image_type: "dockerfile".to_string(),
            },
        );

        // Mismatched hash should return None
        assert!(
            cache
                .get_for_dockerfile("dockerfile:test", "different")
                .is_none()
        );
    }

    #[test]
    fn test_cache_get_for_dockerfile_no_hash() {
        let mut cache = ImageCache::default();

        cache.insert(
            "dockerfile:test".to_string(),
            ImageCacheEntry {
                image_id: "im-def456".to_string(),
                dockerfile_hash: None,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                image_type: "dockerfile".to_string(),
            },
        );

        // No hash in cache should return None
        assert!(
            cache
                .get_for_dockerfile("dockerfile:test", "abc123")
                .is_none()
        );
    }

    #[test]
    fn test_compute_file_hash() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        // Write test content
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(b"Hello, world!").unwrap();
        drop(file);

        // Compute hash
        let hash = compute_file_hash(&file_path).unwrap();

        // Verify hash is 64 hex characters (SHA256)
        assert_eq!(hash.len(), 64);

        // Verify hash is deterministic
        let hash2 = compute_file_hash(&file_path).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_compute_file_hash_nonexistent_file() {
        let result = compute_file_hash(Path::new("/nonexistent/file.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn test_cache_insert_updates_existing() {
        let mut cache = ImageCache::default();

        cache.insert(
            "default".to_string(),
            ImageCacheEntry {
                image_id: "im-abc123".to_string(),
                dockerfile_hash: None,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                image_type: "preset".to_string(),
            },
        );

        // Insert again with different ID
        cache.insert(
            "default".to_string(),
            ImageCacheEntry {
                image_id: "im-xyz789".to_string(),
                dockerfile_hash: None,
                created_at: "2024-01-02T00:00:00Z".to_string(),
                image_type: "preset".to_string(),
            },
        );

        // Should have the new ID
        let entry = cache.get("default").unwrap();
        assert_eq!(entry.image_id, "im-xyz789");
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn test_cache_corrupted_json() {
        let temp_dir = TempDir::new().unwrap();
        let offload_dir = temp_dir.path().join(".offload");
        fs::create_dir_all(&offload_dir).unwrap();

        // Write invalid JSON
        let cache_path = offload_dir.join("modal_images.json");
        fs::write(&cache_path, "{ invalid json }").unwrap();

        // Should return empty cache instead of panicking
        let cache = ImageCache::load(temp_dir.path());
        assert!(cache.entries.is_empty());
    }
}
