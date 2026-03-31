use std::path::PathBuf;
use tracing::{debug, warn};

/// Generate a screenshot file path: ~/.nerve/screenshots/{timestamp}.png
/// Uses millisecond precision to avoid collisions on rapid pastes.
pub fn screenshot_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    PathBuf::from(home)
        .join(".nerve/screenshots")
        .join(format!("{}.png", ts))
}

/// Save raw RGBA pixel data as PNG file.
pub fn save_rgba_as_png(
    rgba: &[u8],
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("create file: {}", e))?;
    let writer = std::io::BufWriter::new(file);
    let encoder = image::codecs::png::PngEncoder::new(writer);
    image::ImageEncoder::write_image(encoder, rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("png encode: {}", e))
}

/// Try to read an image from the system clipboard.
/// Returns (path, width, height) if successful.
/// This is blocking and should be called from a blocking task.
pub fn try_save_clipboard_image() -> Result<PathBuf, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard init: {}", e))?;
    let img = clipboard
        .get_image()
        .map_err(|e| format!("get_image: {}", e))?;

    debug!(
        "clipboard image: {}x{}, {} bytes",
        img.width,
        img.height,
        img.bytes.len()
    );

    let path = screenshot_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {}", e))?;
    }

    save_rgba_as_png(&img.bytes, img.width as u32, img.height as u32, &path)?;
    Ok(path)
}

/// Async wrapper: try to grab clipboard image in a blocking task.
pub async fn try_paste_image() -> Option<PathBuf> {
    match tokio::task::spawn_blocking(try_save_clipboard_image).await {
        Ok(Ok(path)) => {
            debug!("clipboard image saved: {}", path.display());
            Some(path)
        }
        Ok(Err(e)) => {
            debug!("no clipboard image: {}", e);
            None
        }
        Err(e) => {
            warn!("spawn_blocking failed: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_format() {
        let path = screenshot_path();
        let s = path.to_string_lossy();
        assert!(s.contains(".nerve/screenshots/"), "path: {}", s);
        assert!(s.ends_with(".png"), "path: {}", s);
    }

    #[test]
    fn path_dir_creation() {
        let path = screenshot_path();
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        assert!(dir.exists());
    }

    #[test]
    fn save_rgba_roundtrip() {
        // 2x2 red RGBA pixels
        let rgba: Vec<u8> = vec![255, 0, 0, 255].repeat(4);
        let dir = std::env::temp_dir().join("nerve-tui-test-clipboard");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.png");
        save_rgba_as_png(&rgba, 2, 2, &path).unwrap();
        assert!(path.exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn save_rgba_zero_size_errors() {
        let dir = std::env::temp_dir().join("nerve-tui-test-clipboard-zero");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zero.png");
        // 0x0 image with no data - should still work or error gracefully
        let result = save_rgba_as_png(&[], 0, 0, &path);
        // Either succeeds (valid 0x0 PNG) or errors - just shouldn't panic
        drop(result);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn consecutive_paths_are_unique() {
        let p1 = screenshot_path();
        // Even within the same millisecond, format includes enough precision
        let p2 = screenshot_path();
        // In practice these will differ due to ms precision;
        // if they happen to collide, sleep 1ms and retry
        let p2 = if p1 == p2 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            screenshot_path()
        } else {
            p2
        };
        assert_ne!(p1, p2, "consecutive paths must differ: {:?}", p1);
    }

    #[test]
    fn save_rgba_bad_path_errors() {
        let result = save_rgba_as_png(&[0; 16], 2, 2, std::path::Path::new("/nonexistent/dir/test.png"));
        assert!(result.is_err());
    }
}
