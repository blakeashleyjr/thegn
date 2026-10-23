//! Off-loop rasterization of document previews to RGBA (AF 775 graphics path).
//!
//! Called only from `spawn_blocking` (all of these read files and, for PDF /
//! Mermaid, shell out to an external renderer). The pixels are handed to
//! [`crate::graphics::kitty_image`] for display. Every path degrades to a text
//! fallback upstream when it errors, so a missing `pdftoppm` / `mmdc` (or a
//! terminal without graphics) never breaks the preview — it just shows text.
//! No AI/agent dependency anywhere.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

/// A decoded raster ready for [`crate::graphics::kitty_image`].
pub struct Raster {
    /// Original pixels for the fallback encoder; empty when `kitty` is prepared.
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub kitty: Option<std::sync::Arc<crate::graphics::KittyPayload>>,
}

/// Cap the longest visible raster side so the kitty payload (and base64 of it)
/// stays bounded — a preview pane is a few hundred pixels at most. This display
/// cap is separate from the source decode budget in [`PREVIEW_LIMITS`].
const MAX_DIM: u32 = 800;

/// Hard bounds on the SOURCE image, checked from the header before any pixel
/// buffer is allocated. [`MAX_DIM`] only caps the final raster; without these a
/// tiny compressed file declaring enormous dimensions would still be fully
/// decoded first.
#[derive(Clone, Copy)]
pub(crate) struct DecodeLimits {
    max_dim: u32,
    max_pixels: u64,
    max_alloc: u64,
}

/// Keep player supplied cover art to a modest decode budget.
pub(crate) const MEDIA_ART_LIMITS: DecodeLimits = DecodeLimits {
    max_dim: 8192,
    max_pixels: 32 * 1024 * 1024,
    max_alloc: 192 * 1024 * 1024,
};
/// Bound preview source dimensions while allowing high resolution photos. This
/// decode budget is separate from [`MAX_DIM`], which only caps visible output.
const MAX_PREVIEW_INPUT_DIM: u32 = 16384;
/// Bound preview pixel count to 256 MP before decoder allocation.
const MAX_PREVIEW_INPUT_PIXELS: u64 = 256 * 1024 * 1024;
/// Larger camera images remain previewable while the pixel buffer stays bounded to 1 GiB.
const PREVIEW_LIMITS: DecodeLimits = DecodeLimits {
    max_dim: MAX_PREVIEW_INPUT_DIM,
    max_pixels: MAX_PREVIEW_INPUT_PIXELS,
    max_alloc: 1024 * 1024 * 1024,
};
/// Bound preview file reads to 256 MiB while covering large camera JPEGs.
const MAX_PREVIEW_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Mirrors the 2 MiB text-preview cap in `preview_pane.rs::read_capped`, which
/// is what users see when Mermaid rendering fails.
const MAX_MERMAID_INPUT_BYTES: u64 = 2 * 1024 * 1024;
/// Bound generated Mermaid PNGs independently of the decoder's pixel budget.
const MAX_MERMAID_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;

/// Owns all files visible to one renderer process. The tempdir is private and
/// removed when the renderer has returned and this value is dropped.
struct RenderStage {
    dir: tempfile::TempDir,
}

impl RenderStage {
    fn new() -> Result<Self, String> {
        let dir = tempfile::Builder::new()
            .prefix("thegn-preview-")
            .tempdir()
            .map_err(|e| format!("preview staging failed: {e}"))?;
        std::fs::create_dir(dir.path().join("home"))
            .map_err(|e| format!("preview staging failed: {e}"))?;
        std::fs::create_dir(dir.path().join("tmp"))
            .map_err(|e| format!("preview staging failed: {e}"))?;
        Ok(Self { dir })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Copy a previously opened, bounded regular input into a fresh staging file.
fn stage_input(
    stage: &RenderStage,
    source: &Path,
    name: &str,
    cap: u64,
) -> Result<PathBuf, String> {
    use std::io::Read;

    let file = open_regular_nonblocking(source)?;
    let metadata = file.metadata().map_err(|e| format!("read failed: {e}"))?;
    if metadata.len() > cap {
        return Err(format!("file exceeds the {cap} byte preview cap"));
    }

    let staged = stage.path().join(name);
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|e| format!("staged input create failed: {e}"))?;
    let copied = std::io::copy(&mut file.take(cap.saturating_add(1)), &mut destination)
        .map_err(|e| format!("staged input copy failed: {e}"))?;
    if copied > cap {
        drop(destination);
        if let Err(error) = std::fs::remove_file(&staged) {
            tracing::debug!(%error, path = %staged.display(), "failed to remove over-cap staged input");
        }
        return Err(format!(
            "file exceeds the {cap} byte preview cap (more than {cap} bytes)"
        ));
    }
    Ok(staged)
}

/// Keep only renderer inputs that are safe and needed. PATH entries must be
/// absolute so the renderer cannot resolve tools relative to a workspace.
fn renderer_env<I>(host: I, stage: &Path) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut allowed = Vec::new();
    let mut host_home = None;
    let mut has_puppeteer_cache = false;
    for (key, value) in host {
        let key_text = key.to_string_lossy().into_owned();
        if key_text.eq_ignore_ascii_case("HOME") {
            host_home = Some(value.clone());
        }
        if key_text.eq_ignore_ascii_case("PUPPETEER_CACHE_DIR") {
            has_puppeteer_cache = true;
        }
        if key_text.eq_ignore_ascii_case("PATH") {
            let absolute: Vec<_> = std::env::split_paths(&value)
                .filter(|entry| entry.is_absolute())
                .collect();
            allowed.push((
                OsString::from("PATH"),
                std::env::join_paths(absolute).unwrap_or_default(),
            ));
        } else if key_text.eq_ignore_ascii_case("LANG")
            || key_text.eq_ignore_ascii_case("TZ")
            || key_text.to_ascii_uppercase().starts_with("LC_")
            || key_text.eq_ignore_ascii_case("PUPPETEER_EXECUTABLE_PATH")
            || key_text.eq_ignore_ascii_case("PUPPETEER_CACHE_DIR")
        {
            allowed.push((key, value));
        } else if key_text.eq_ignore_ascii_case("SYSTEMROOT") {
            allowed.push((OsString::from("SystemRoot"), value));
        } else if key_text.eq_ignore_ascii_case("PATHEXT") {
            allowed.push((OsString::from("PATHEXT"), value));
        } else if key_text.eq_ignore_ascii_case("COMSPEC") {
            allowed.push((OsString::from("COMSPEC"), value));
        }
    }

    if !has_puppeteer_cache && let Some(home) = host_home {
        allowed.push((
            OsString::from("PUPPETEER_CACHE_DIR"),
            Path::new(&home).join(".cache/puppeteer").into_os_string(),
        ));
    }

    let home = stage.join("home");
    allowed.extend([
        (OsString::from("HOME"), home.clone().into_os_string()),
        (
            OsString::from("XDG_CONFIG_HOME"),
            home.join(".config").into_os_string(),
        ),
        (
            OsString::from("XDG_CACHE_HOME"),
            home.join(".cache").into_os_string(),
        ),
        (
            OsString::from("XDG_DATA_HOME"),
            home.join(".local/share").into_os_string(),
        ),
        (OsString::from("TMPDIR"), stage.join("tmp").into_os_string()),
    ]);
    allowed
}

/// Why a renderer binary was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RendererLookup {
    /// No absolute PATH entry holds the program.
    Missing,
    /// Every candidate lives inside the previewed repository (first refused candidate kept for the message).
    Shadowed(PathBuf),
}

impl RendererLookup {
    fn describe(&self, program: &str) -> String {
        match self {
            RendererLookup::Missing => format!("{program} not found on PATH"),
            RendererLookup::Shadowed(path) => format!(
                "{program} at {} is inside the previewed repository; refusing to run it",
                path.display()
            ),
        }
    }
}

/// The nearest ancestor of `previewed` (a file) that holds a `.git` entry (directory or gitfile):
/// the repository or worktree whose content is being previewed. `None` outside any repository.
fn untrusted_root_for(previewed: &Path) -> Option<PathBuf> {
    let start = previewed.canonicalize().ok()?;
    let dir = start.parent()?;
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").symlink_metadata().is_ok())
        .map(Path::to_path_buf)
}

/// Resolve `program` through absolute PATH entries, skipping candidates in the untrusted repository.
fn resolve_renderer(
    program: &str,
    path_var: Option<&OsStr>,
    untrusted_root: Option<&Path>,
) -> Result<PathBuf, RendererLookup> {
    let Some(path_var) = path_var else {
        return Err(RendererLookup::Missing);
    };
    let root =
        untrusted_root.map(|root| root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let mut first_shadowed = None;
    for entry in std::env::split_paths(path_var).filter(|entry| entry.is_absolute()) {
        let candidate = entry.join(program);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file()
            || crate::platform::gate_path::executable_bit(&metadata) == Some(false)
        {
            continue;
        }
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        let entry_canon = entry.canonicalize().unwrap_or_else(|_| entry.clone());
        let is_shadowed = root
            .as_ref()
            .is_some_and(|root| canonical.starts_with(root) || entry_canon.starts_with(root));
        if is_shadowed {
            first_shadowed.get_or_insert(candidate);
            continue;
        }
        return Ok(candidate);
    }
    match first_shadowed {
        Some(path) => Err(RendererLookup::Shadowed(path)),
        None => Err(RendererLookup::Missing),
    }
}

fn renderer_for(program: &str, previewed: &Path) -> Result<PathBuf, RendererLookup> {
    resolve_renderer(
        program,
        std::env::var_os("PATH").as_deref(),
        untrusted_root_for(previewed).as_deref(),
    )
}

/// `program` is resolved by `renderer_for`, which refuses a renderer inside the previewed repository.
fn renderer_command(program: &Path, stage: &RenderStage) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(renderer_env(std::env::vars_os(), stage.path()))
        .current_dir(stage.path());
    command
}

/// Whether declared source dimensions are within the decode budget. Pure.
fn check_dimensions(w: u32, h: u32, limits: DecodeLimits) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err("image has no pixels".to_string());
    }
    if w > limits.max_dim || h > limits.max_dim {
        return Err(format!("image dimensions {w}x{h} exceed the decode limit"));
    }
    if u64::from(w) * u64::from(h) > limits.max_pixels {
        return Err(format!("image {w}x{h} exceeds the pixel limit"));
    }
    Ok(())
}

/// Decode encoded image `bytes` to RGBA, downscaling to fit [`MAX_DIM`]. The
/// header dimensions are validated and decoder limits installed first.
fn decode_and_fit(bytes: &[u8], limits: DecodeLimits) -> Result<Raster, String> {
    let reader = || {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| format!("image decode failed: {e}"))
    };
    let (w, h) = reader()?
        .into_dimensions()
        .map_err(|e| format!("image decode failed: {e}"))?;
    check_dimensions(w, h, limits)?;
    let mut r = reader()?;
    let mut decoder_limits = image::Limits::default();
    decoder_limits.max_image_width = Some(limits.max_dim);
    decoder_limits.max_image_height = Some(limits.max_dim);
    decoder_limits.max_alloc = Some(limits.max_alloc);
    r.limits(decoder_limits);
    let img = r
        .decode()
        .map_err(|e| format!("image decode failed: {e}"))?;
    let img = if img.width() > MAX_DIM || img.height() > MAX_DIM {
        img.thumbnail(MAX_DIM, MAX_DIM)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Raster {
        rgba: rgba.into_raw(),
        w,
        h,
        kitty: None,
    })
}

/// Read at most `cap` bytes from a REGULAR file. The kind is verified on the
/// opened handle (not just the path), the resolved final target is opened
/// without following a symlink, the open cannot block on a FIFO, and `take`
/// bounds a file that grows mid-read.
pub(crate) fn read_regular_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let file = open_regular_nonblocking(path)?;
    read_open_regular_capped(file, cap)
}

pub(crate) fn read_open_regular_capped(file: std::fs::File, cap: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let meta = file.metadata().map_err(|e| format!("read failed: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > cap {
        return Err(format!(
            "file exceeds the {cap} byte preview cap ({} bytes)",
            meta.len()
        ));
    }
    let mut buf = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    if buf.len() as u64 > cap {
        return Err(format!(
            "file exceeds the {cap} byte preview cap (more than {cap} bytes)"
        ));
    }
    Ok(buf)
}

/// Resolve a caller path (allowing a final symlink), then open the resolved
/// path without following a final symlink. Validate the opened handle too.
/// Nonblocking open prevents a path swapped to a FIFO from parking the caller.
fn open_regular_nonblocking(path: &Path) -> Result<std::fs::File, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("read failed: {e}"))?;
    match std::fs::metadata(&canonical) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("not a regular file".to_string()),
        Err(e) => return Err(format!("read failed: {e}")),
    }
    let file = crate::platform::open_read_nonblocking_nofollow(&canonical)
        .map_err(|e| format!("read failed: {e}"))?;
    if !file
        .metadata()
        .map_err(|e| format!("read failed: {e}"))?
        .is_file()
    {
        return Err("not a regular file".to_string());
    }
    Ok(file)
}

/// Read a small prefix from the opened regular file. Used only for
/// extension-less content sniffing.
pub(crate) fn read_regular_prefix(path: &Path, cap: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = open_regular_nonblocking(path)?;
    let mut prefix = Vec::with_capacity(cap);
    file.take(cap as u64)
        .read_to_end(&mut prefix)
        .map_err(|e| format!("read failed: {e}"))?;
    Ok(prefix)
}

/// Rasterize an image file (PNG/JPEG) to RGBA. Blocking.
pub fn image_file(path: &Path) -> Result<Raster, String> {
    image_file_with_limits(path, MAX_PREVIEW_FILE_BYTES, PREVIEW_LIMITS)
}

/// Rasterize a file with caller-chosen byte and decode limits. Blocking.
pub(crate) fn image_file_with_limits(
    path: &Path,
    cap: u64,
    limits: DecodeLimits,
) -> Result<Raster, String> {
    let bytes = read_regular_capped(path, cap)?;
    decode_and_fit(&bytes, limits)
}

/// Decode bytes with caller-chosen limits (cover art uses the tighter budget).
pub(crate) fn image_bytes_with_limits(
    bytes: &[u8],
    limits: DecodeLimits,
) -> Result<Raster, String> {
    decode_and_fit(bytes, limits)
}

/// Rasterize page 1 of a PDF via `pdftoppm` (poppler) → PNG on stdout → RGBA.
/// Errs (→ text fallback) when the tool is absent, fails, exceeds the renderer
/// deadline or `cancel` is raised; the whole process group is killed and
/// reaped on every path. Blocking.
pub fn pdf_page1(path: &Path, cancel: &AtomicBool) -> Result<Raster, String> {
    let stage = RenderStage::new()?;
    let staged = stage_input(&stage, path, "input.pdf", MAX_PREVIEW_FILE_BYTES)
        .map_err(|e| format!("preview staging failed: {e}"))?;
    let program = renderer_for("pdftoppm", path)
        .map_err(|e| format!("pdftoppm: {}", e.describe("pdftoppm")))?;
    let mut cmd = renderer_command(&program, &stage);
    cmd.args(pdftoppm_args(&staged));
    let out = crate::preview_jobs::run_renderer(&mut cmd, cancel)
        .map_err(|e| format!("pdftoppm: {}", capture_err(&e)))?;
    decode_and_fit(&out, PREVIEW_LIMITS)
}

/// Bound pdftoppm's own page bitmap allocation. The visible raster is at most
/// [`MAX_DIM`] too, but post-render decoding cannot protect the renderer's peak.
fn pdftoppm_args(path: &Path) -> Vec<OsString> {
    ["-png", "-f", "1", "-l", "1", "-singlefile", "-scale-to"]
        .into_iter()
        .map(OsString::from)
        .chain([
            OsString::from(MAX_DIM.to_string()),
            path.as_os_str().to_owned(),
            OsString::from("-"),
        ])
        .collect()
}

/// Extract a PDF's text via `pdftotext` (the no-graphics fallback). Output cut
/// at the byte cap is returned truncated (fine for text); `Err` carries a
/// truthful reason (absent tool, failure, timeout, cancel). Blocking.
pub fn pdf_text(path: &Path, cancel: &AtomicBool) -> Result<String, String> {
    use crate::preview_jobs::CaptureError as C;
    let stage = RenderStage::new()?;
    let staged = stage_input(&stage, path, "input.pdf", MAX_PREVIEW_FILE_BYTES)
        .map_err(|e| format!("preview staging failed: {e}"))?;
    let program = match renderer_for("pdftotext", path) {
        Ok(program) => program,
        Err(RendererLookup::Missing) => return Err("no text extractor available".to_string()),
        Err(error) => return Err(error.describe("pdftotext")),
    };
    let mut cmd = renderer_command(&program, &stage);
    cmd.args(pdftotext_args(&staged));
    match crate::preview_jobs::run_renderer(&mut cmd, cancel) {
        Ok(out) | Err(C::Capped(out)) => Ok(String::from_utf8_lossy(&out).into_owned()),
        Err(C::Spawn(_)) => Err("no text extractor available".to_string()),
        Err(e) => Err(format!("text extraction {}", capture_err(&e))),
    }
}

fn pdftotext_args(path: &Path) -> Vec<OsString> {
    [path.as_os_str().to_os_string(), OsString::from("-")].into()
}

/// Render a Mermaid file via `mmdc` (mermaid-cli) → PNG → RGBA. Errs (→ source
/// fallback) when the tool is absent, fails, times out or is cancelled; the
/// renderer's whole process tree (mmdc spawns a browser) is killed and reaped.
/// Blocking.
pub fn mermaid(path: &Path, cancel: &AtomicBool) -> Result<Raster, String> {
    let stage = RenderStage::new()?;
    let staged = stage_input(&stage, path, "input.mmd", MAX_MERMAID_INPUT_BYTES)
        .map_err(|e| format!("preview staging failed: {e}"))?;
    let tmp = stage.path().join("render.png");
    let program =
        renderer_for("mmdc", path).map_err(|e| format!("mmdc: {}", e.describe("mmdc")))?;
    let mut cmd = renderer_command(&program, &stage);
    cmd.args(mermaid_args(&staged, &tmp));
    match crate::preview_jobs::run_renderer_quiet(&mut cmd, cancel) {
        Ok(()) => read_mermaid_output(stage.path(), "render.png", MAX_MERMAID_OUTPUT_BYTES),
        Err(e) => Err(format!("mmdc: {}", capture_err(&e))),
    }
}

fn mermaid_args(input: &Path, output: &Path) -> Vec<OsString> {
    [
        OsString::from("-i"),
        input.as_os_str().to_os_string(),
        OsString::from("-o"),
        output.as_os_str().to_os_string(),
        OsString::from("-b"),
        OsString::from("transparent"),
    ]
    .into()
}

fn read_mermaid_output(stage_root: &Path, name: &str, cap: u64) -> Result<Raster, String> {
    let file = crate::platform::open_beneath::open_regular_beneath(stage_root, Path::new(name))
        .map_err(|e| format!("mmdc output read failed: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("mmdc output read failed: {e}"))?;
    thegn_core::fsperm::ensure_single_link_regular_file(&meta)
        .map_err(|e| format!("mmdc output read failed: {e}"))?;
    if !thegn_core::fsperm::owned_by_current_user(&meta) {
        return Err("mmdc output read failed: not owned by the current user".to_string());
    }
    let bytes =
        read_open_regular_capped(file, cap).map_err(|e| format!("mmdc output read failed: {e}"))?;

    decode_and_fit(&bytes, PREVIEW_LIMITS)
}

fn capture_err(e: &crate::preview_jobs::CaptureError) -> String {
    use crate::preview_jobs::CaptureError as C;
    match e {
        C::Spawn(err) => format!("unavailable: {err}"),
        C::Cancelled => "cancelled".to_string(),
        C::Timeout => "timed out".to_string(),
        C::Failed => "failed".to_string(),
        C::Capped(_) => "output exceeded the size cap".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RendererFixture {
        repo: PathBuf,
        repo_bin: PathBuf,
        system: PathBuf,
        other: PathBuf,
    }

    fn renderer_fixture(root: &Path) -> RendererFixture {
        let repo = root.join("repo");
        let repo_bin = repo.join("bin");
        let system = root.join("system");
        let other = root.join("other");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&repo_bin).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(repo_bin.join("pdftoppm"), b"repo renderer").unwrap();
        std::fs::write(system.join("pdftoppm"), b"system renderer").unwrap();
        std::fs::write(other.join("pdftoppm"), b"other renderer").unwrap();
        crate::platform::set_owner_executable(&repo_bin.join("pdftoppm")).unwrap();
        crate::platform::set_owner_executable(&system.join("pdftoppm")).unwrap();
        crate::platform::set_owner_executable(&other.join("pdftoppm")).unwrap();
        std::fs::write(repo.join("doc.pdf"), b"pdf").unwrap();
        RendererFixture {
            repo,
            repo_bin,
            system,
            other,
        }
    }

    #[test]
    fn renderer_resolution_skips_a_shadow_inside_the_previewed_repo() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = renderer_fixture(temp.path());
        let path = std::env::join_paths([fixture.repo_bin.as_os_str(), fixture.system.as_os_str()])
            .unwrap();
        let root = untrusted_root_for(&fixture.repo.join("doc.pdf"));
        assert_eq!(
            resolve_renderer("pdftoppm", Some(&path), root.as_deref()),
            Ok(fixture.system.join("pdftoppm"))
        );
    }

    #[test]
    fn renderer_resolution_refuses_when_only_a_repo_shadow_exists() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = renderer_fixture(temp.path());
        let path = std::env::join_paths([fixture.repo_bin.as_os_str()]).unwrap();
        let result = resolve_renderer(
            "pdftoppm",
            Some(&path),
            untrusted_root_for(&fixture.repo.join("doc.pdf")).as_deref(),
        );
        assert_eq!(
            result,
            Err(RendererLookup::Shadowed(fixture.repo_bin.join("pdftoppm")))
        );
        assert!(
            result
                .unwrap_err()
                .describe("pdftoppm")
                .contains("inside the previewed repository")
        );
    }

    #[test]
    fn renderer_resolution_refuses_a_symlink_into_the_repo() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let fixture = renderer_fixture(temp.path());
        let system = temp.path().join("symlink-bin");
        std::fs::create_dir(&system).unwrap();
        let link = system.join("pdftoppm");
        if crate::platform::test_symlink(&fixture.repo_bin.join("pdftoppm"), &link).is_err() {
            return;
        }
        let path = std::env::join_paths([system.as_os_str()]).unwrap();
        assert_eq!(
            resolve_renderer(
                "pdftoppm",
                Some(&path),
                untrusted_root_for(&fixture.repo.join("doc.pdf")).as_deref()
            ),
            Err(RendererLookup::Shadowed(link))
        );
    }

    #[test]
    fn renderer_resolution_untrusted_root_is_the_nearest_git_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let nested = repo.join("a/b");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let doc = nested.join("doc.pdf");
        std::fs::write(&doc, b"pdf").unwrap();
        assert_eq!(untrusted_root_for(&doc), Some(repo.canonicalize().unwrap()));

        let repo_with_gitfile = temp.path().join("repo-with-gitfile");
        std::fs::create_dir_all(&repo_with_gitfile).unwrap();
        std::fs::write(repo_with_gitfile.join(".git"), b"gitdir: /elsewhere").unwrap();
        let gitfile_doc = repo_with_gitfile.join("doc.pdf");
        std::fs::write(&gitfile_doc, b"pdf").unwrap();
        assert_eq!(
            untrusted_root_for(&gitfile_doc),
            Some(repo_with_gitfile.canonicalize().unwrap())
        );
    }

    #[test]
    fn renderer_resolution_without_a_root_uses_the_first_absolute_entry() {
        // Positive control: without a repository root, the first absolute PATH hit remains selected.
        let temp = tempfile::tempdir().unwrap();
        let fixture = renderer_fixture(temp.path());
        let path = std::env::join_paths([
            std::ffi::OsStr::new("relative/bin"),
            fixture.system.as_os_str(),
            fixture.other.as_os_str(),
        ])
        .unwrap();
        assert_eq!(
            resolve_renderer("pdftoppm", Some(&path), None),
            Ok(fixture.system.join("pdftoppm"))
        );
        let missing_path = std::env::join_paths([temp.path().as_os_str()]).unwrap();
        assert_eq!(
            resolve_renderer("absent-renderer", Some(&missing_path), None),
            Err(RendererLookup::Missing)
        );
    }

    #[test]
    fn renderer_resolution_skips_a_non_executable_path_hit() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = renderer_fixture(temp.path());
        let earlier = temp.path().join("earlier");
        std::fs::create_dir(&earlier).unwrap();
        let non_executable = earlier.join("pdftoppm");
        std::fs::write(&non_executable, b"not executable").unwrap();
        let metadata = std::fs::metadata(&non_executable).unwrap();
        let path = std::env::join_paths([earlier.as_os_str(), fixture.system.as_os_str()]).unwrap();

        let expected = match crate::platform::gate_path::executable_bit(&metadata) {
            Some(false) => fixture.system.join("pdftoppm"),
            Some(true) | None => non_executable,
        };
        assert_eq!(
            resolve_renderer("pdftoppm", Some(&path), None),
            Ok(expected),
            "Unix PATH lookup skips a regular file without any execute bit"
        );
    }

    #[test]
    fn renderer_env_drops_credentials_and_stages_home() {
        let stage = tempfile::tempdir().unwrap();
        let absolute_path = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let host_path = std::env::join_paths([
            OsString::from("relative-bin"),
            absolute_path.clone().into_os_string(),
        ])
        .unwrap();
        let host = vec![
            (OsString::from("HOME"), OsString::from("/real/home")),
            (
                OsString::from("SSH_AUTH_SOCK"),
                OsString::from("/secret/socket"),
            ),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
            (
                OsString::from("ANTHROPIC_API_KEY"),
                OsString::from("secret"),
            ),
            (
                OsString::from("THEGN_CONTROL_SOCKET"),
                OsString::from("secret"),
            ),
            (OsString::from("LD_PRELOAD"), OsString::from("evil.so")),
            (OsString::from("Path"), host_path),
            (OsString::from("LANG"), OsString::from("en_US.UTF-8")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("SystemRoot"), OsString::from("C:\\Windows")),
            (OsString::from("PATHEXT"), OsString::from(".COM;.EXE;.BAT")),
            (
                OsString::from("COMSPEC"),
                OsString::from("C:\\Windows\\System32\\cmd.exe"),
            ),
        ];
        let env = renderer_env(host, stage.path());
        let keys: std::collections::BTreeSet<_> = env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            keys,
            [
                "PATH",
                "LANG",
                "LC_ALL",
                "SystemRoot",
                "PATHEXT",
                "COMSPEC",
                "PUPPETEER_CACHE_DIR",
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "TMPDIR",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| key == std::ffi::OsStr::new(name))
                .map(|(_, value)| value)
                .expect("expected allowlisted environment value")
        };
        assert!(Path::new(value("HOME")).starts_with(stage.path()));
        assert_eq!(value("PATH").as_os_str(), absolute_path.as_os_str());
        assert_eq!(value("SystemRoot"), std::ffi::OsStr::new("C:\\Windows"));
        assert_eq!(value("PATHEXT"), std::ffi::OsStr::new(".COM;.EXE;.BAT"));
        assert_eq!(
            value("COMSPEC"),
            std::ffi::OsStr::new("C:\\Windows\\System32\\cmd.exe")
        );
        let puppeteer_cache = Path::new("/real/home")
            .join(".cache/puppeteer")
            .into_os_string();
        assert_eq!(
            value("PUPPETEER_CACHE_DIR").as_os_str(),
            puppeteer_cache.as_os_str()
        );
    }

    #[test]
    fn renderer_child_sees_only_the_allowlist() {
        if cfg!(windows) {
            return;
        }
        let stage = RenderStage::new().unwrap();
        let mut command = renderer_command(Path::new("/bin/sh"), &stage);
        command.arg("-c").arg("pwd; env");
        let output =
            crate::preview_jobs::run_renderer(&mut command, &AtomicBool::new(false)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.lines().any(|line| {
            line.starts_with("SSH_AUTH_SOCK=")
                || line.starts_with("THEGN_")
                || line.split_once('=').is_some_and(|(key, _)| {
                    key.starts_with("GPG_") || key.ends_with("_TOKEN") || key.ends_with("_API_KEY")
                })
        }));
        let canonical_stage = stage.path().canonicalize().unwrap();
        assert_eq!(
            output.lines().next(),
            Some(canonical_stage.to_str().unwrap())
        );
        let home = format!("HOME={}", stage.path().join("home").display());
        assert!(output.lines().any(|line| line == home));
    }

    #[test]
    fn renderer_input_is_a_staged_copy() {
        let source_dir = tempfile::tempdir().unwrap();
        let stage = RenderStage::new().unwrap();
        let source = source_dir.path().join("document.pdf");
        let canary = source_dir.path().join("canary.pdf");
        std::fs::write(&source, b"original document").unwrap();
        std::fs::write(&canary, b"canary document").unwrap();

        let staged = stage_input(&stage, &source, "input.pdf", 64).unwrap();
        let staged_os = staged.as_os_str().to_os_string();
        let source_os = source.as_os_str().to_os_string();
        let pdf_image_args = pdftoppm_args(&staged);
        let pdf_text_args = pdftotext_args(&staged);
        let mermaid_output = stage.path().join("render.png");
        let mermaid_renderer_args = mermaid_args(&staged, &mermaid_output);
        for args in [pdf_image_args, pdf_text_args, mermaid_renderer_args] {
            assert!(args.contains(&staged_os));
            assert!(!args.contains(&source_os));
        }
        std::fs::remove_file(&source).unwrap();
        if crate::platform::symlink_file_for_test(&canary, &source).is_err() {
            return;
        }
        let bytes = std::fs::read(&staged).unwrap();
        assert_eq!(bytes, b"original document");
        assert!(staged.starts_with(stage.path()));
        assert!(
            !bytes
                .windows(b"canary document".len())
                .any(|window| { window == b"canary document" })
        );
    }

    #[test]
    fn renderer_input_over_cap_is_refused() {
        let source_dir = tempfile::tempdir().unwrap();
        let stage = RenderStage::new().unwrap();
        let source = source_dir.path().join("too-large.mmd");
        std::fs::write(&source, [b'x'; 17]).unwrap();
        assert!(stage_input(&stage, &source, "input.mmd", 16).is_err());
        assert!(std::fs::read_dir(stage.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("input.")
        }));
    }

    #[test]
    fn renderer_stage_is_removed_after_drop() {
        let stage = RenderStage::new().unwrap();
        let path = stage.path().to_path_buf();
        drop(stage);
        assert!(!path.exists());
    }

    #[test]
    fn decodes_a_generated_png_to_rgba() {
        // Encode a 2x3 red PNG in-memory, then round-trip through decode_and_fit.
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let r = decode_and_fit(buf.get_ref(), PREVIEW_LIMITS).unwrap();
        assert_eq!((r.w, r.h), (2, 3));
        assert_eq!(r.rgba.len(), 2 * 3 * 4);
        assert_eq!(&r.rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn downscales_oversized_images_under_the_cap() {
        let img = image::RgbaImage::from_pixel(2000, 1000, image::Rgba([0, 0, 0, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let r = decode_and_fit(buf.get_ref(), PREVIEW_LIMITS).unwrap();
        assert!(r.w <= MAX_DIM && r.h <= MAX_DIM);
        assert!(
            r.w == MAX_DIM || r.h == MAX_DIM,
            "longest side hits the cap"
        );
    }

    #[test]
    fn thumbnail_preserves_extreme_non_square_aspect_ratio() {
        let img = image::RgbaImage::from_pixel(3000, 7, image::Rgba([0, 0, 0, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let raster = decode_and_fit(buf.get_ref(), PREVIEW_LIMITS).unwrap();
        assert_eq!(raster.w, MAX_DIM);
        assert!(raster.h >= 1 && raster.h <= MAX_DIM);
    }

    #[test]
    fn pdftoppm_arguments_bound_the_renderer_bitmap() {
        let path = Path::new("document.pdf");
        let args = pdftoppm_args(path);
        let args: Vec<&std::ffi::OsStr> = args.iter().map(OsString::as_os_str).collect();
        assert!(args.windows(2).any(|pair| {
            pair[0] == std::ffi::OsStr::new("-scale-to") && pair[1] == std::ffi::OsStr::new("800")
        }));
        assert!(args.windows(2).any(|pair| {
            pair[0] == std::ffi::OsStr::new("-f") && pair[1] == std::ffi::OsStr::new("1")
        }));
        assert!(args.windows(2).any(|pair| {
            pair[0] == std::ffi::OsStr::new("-l") && pair[1] == std::ffi::OsStr::new("1")
        }));
        assert!(args.contains(&std::ffi::OsStr::new("-singlefile")));
        assert!(!args.contains(&std::ffi::OsStr::new("-r")));
        assert_eq!(
            &args[args.len() - 2..],
            [path.as_os_str(), std::ffi::OsStr::new("-")]
        );
    }

    #[test]
    fn dimension_guard_bounds_width_height_and_pixels() {
        assert!(check_dimensions(800, 800, MEDIA_ART_LIMITS).is_ok());
        assert!(check_dimensions(MEDIA_ART_LIMITS.max_dim, 1, MEDIA_ART_LIMITS).is_ok());
        assert!(check_dimensions(0, 10, MEDIA_ART_LIMITS).is_err());
        assert!(check_dimensions(MEDIA_ART_LIMITS.max_dim + 1, 1, MEDIA_ART_LIMITS).is_err());
        assert!(check_dimensions(1, MEDIA_ART_LIMITS.max_dim + 1, MEDIA_ART_LIMITS).is_err());
        // Each side is legal but the area is not.
        assert!(
            check_dimensions(
                MEDIA_ART_LIMITS.max_dim,
                MEDIA_ART_LIMITS.max_dim,
                MEDIA_ART_LIMITS
            )
            .is_err()
        );
        assert!(check_dimensions(MAX_PREVIEW_INPUT_DIM, 1, PREVIEW_LIMITS).is_ok());
        for limits in [MEDIA_ART_LIMITS, PREVIEW_LIMITS] {
            assert!(check_dimensions(u32::MAX, u32::MAX, limits).is_err());
            assert!(check_dimensions(u32::MAX, 1, limits).is_err());
        }
        assert!(
            check_dimensions(
                MAX_PREVIEW_INPUT_DIM,
                MAX_PREVIEW_INPUT_PIXELS as u32 / MAX_PREVIEW_INPUT_DIM,
                PREVIEW_LIMITS
            )
            .is_ok()
        );
    }

    #[test]
    fn dimension_bomb_png_is_refused_before_decode() {
        // 1x1 PNG with the IHDR rewritten to 60000x60000: tiny file, huge claim.
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let mut png = buf.into_inner();
        png[16..20].copy_from_slice(&60000u32.to_be_bytes());
        png[20..24].copy_from_slice(&60000u32.to_be_bytes());
        let crc = crc32fast::hash(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let err = decode_and_fit(&png, MEDIA_ART_LIMITS)
            .err()
            .expect("must refuse");
        assert!(err.contains("dimensions 60000x60000 exceed the decode limit"));
    }

    #[test]
    fn truncated_png_is_rejected() {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let mut png = buf.into_inner();
        png.truncate(33); // complete signature and IHDR, with IDAT absent
        assert!(decode_and_fit(&png, PREVIEW_LIMITS).is_err());
    }

    #[test]
    fn repeated_dimension_bomb_failures_are_stable() {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let mut png = buf.into_inner();
        png[16..20].copy_from_slice(&60000u32.to_be_bytes());
        png[20..24].copy_from_slice(&60000u32.to_be_bytes());
        let crc = crc32fast::hash(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let first = decode_and_fit(&png, PREVIEW_LIMITS)
            .err()
            .expect("dimension bomb must be rejected");
        for _ in 0..50 {
            let error = decode_and_fit(&png, PREVIEW_LIMITS)
                .err()
                .expect("dimension bomb must be rejected");
            assert_eq!(error, first);
        }
    }

    #[test]
    fn read_regular_capped_enforces_cap_and_kind() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let f = dir.join("f.bin");
        std::fs::write(&f, [7u8; 100]).unwrap();
        assert_eq!(read_regular_capped(&f, 100).unwrap().len(), 100);
        assert!(read_regular_capped(&f, 99).is_err());
        assert!(read_regular_capped(dir, 100).is_err(), "directory refused");
        if crate::platform::test_symlink_supported() {
            let target = dir.join("target");
            let link = dir.join("link");
            std::fs::write(&target, b"secret").unwrap();
            crate::platform::test_symlink(&target, &link).unwrap();
            assert_eq!(read_regular_capped(&link, 100).unwrap(), b"secret");
        }
        if crate::platform::test_fifo_supported() {
            let fifo = dir.join("p");
            crate::platform::test_fifo(&fifo).unwrap();
            // Must return promptly instead of blocking on the FIFO.
            assert!(read_regular_capped(&fifo, 100).is_err());
        }
    }

    #[test]
    fn exact_read_refuses_a_final_symlink_while_preview_read_resolves_it() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.png");
        let inside = dir.path().join("inside.png");
        std::fs::write(&outside, b"bounded fixture").unwrap();
        crate::platform::test_symlink(&outside, &inside).unwrap();

        assert!(
            crate::platform::open_beneath::open_regular_beneath(
                dir.path(),
                Path::new("inside.png")
            )
            .is_err()
        );
        assert_eq!(
            read_regular_capped(&inside, 100).unwrap(),
            b"bounded fixture"
        );
    }

    #[test]
    fn exact_read_refuses_fifo_without_blocking() {
        if !crate::platform::test_fifo_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("cover.fifo");
        crate::platform::test_fifo(&fifo).unwrap();
        assert!(
            crate::platform::open_beneath::open_regular_beneath(
                dir.path(),
                Path::new("cover.fifo")
            )
            .is_err()
        );
    }

    #[test]
    fn mermaid_output_is_refused_at_its_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("render.png");
        std::fs::write(&output, [0u8; 33]).unwrap();
        let error = read_mermaid_output(dir.path(), "render.png", 32)
            .err()
            .expect("must refuse");
        assert!(error.contains("mmdc output read failed: file exceeds the 32 byte preview cap"));
    }

    fn generated_test_png() -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn mermaid_output_symlink_is_refused_and_its_target_never_read() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let stage = RenderStage::new().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("valid.png");
        std::fs::write(&target, generated_test_png()).unwrap();
        let output = stage.path().join("render.png");
        crate::platform::test_symlink(&target, &output).unwrap();

        let result = read_mermaid_output(stage.path(), "render.png", MAX_MERMAID_OUTPUT_BYTES);
        assert!(matches!(result, Err(error) if error.starts_with("mmdc output read failed")));
    }

    #[test]
    fn mermaid_output_hardlink_is_refused_where_link_counts_exist() {
        let stage = RenderStage::new().unwrap();
        let real = stage.path().join("real.png");
        let link = stage.path().join("render.png");
        std::fs::write(&real, generated_test_png()).unwrap();
        std::fs::hard_link(&real, &link).unwrap();

        if thegn_core::fsperm::extra_hard_links(&std::fs::metadata(&link).unwrap()) > 0 {
            let result = read_mermaid_output(stage.path(), "render.png", MAX_MERMAID_OUTPUT_BYTES);
            assert!(matches!(result, Err(error) if error.contains("hardlink")));
        } else {
            assert!(
                read_mermaid_output(stage.path(), "render.png", MAX_MERMAID_OUTPUT_BYTES).is_ok()
            );
        }
    }

    #[test]
    fn mermaid_output_regular_owned_png_still_decodes() {
        // Positive control: the ordinary owned, single-link output already worked before this fix.
        let stage = RenderStage::new().unwrap();
        std::fs::write(stage.path().join("render.png"), generated_test_png()).unwrap();

        let raster =
            read_mermaid_output(stage.path(), "render.png", MAX_MERMAID_OUTPUT_BYTES).unwrap();
        assert_eq!((raster.w, raster.h), (2, 3));
    }

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode_and_fit(b"not an image", PREVIEW_LIMITS).is_err());
    }
}
