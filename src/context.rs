use crate::provider::ImageAttachment;
use anyhow::{Context, Result};
use base64::Engine;
use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

const INSTRUCTION_FILES: [&str; 2] = ["KAMUI.md", "AGENTS.md"];
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 25_000_000;
const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_DIRECTORY_FILES: usize = 50;

/// A prompt after `@` references are expanded: text context inlined, images carried separately.
#[derive(Debug, Default)]
pub struct Expanded {
    pub text: String,
    pub images: Vec<ImageAttachment>,
    /// Files that made it in, and files a directory reference had to leave out. The omissions
    /// were already noted for the model, but not for the person who typed `@src` and got twelve
    /// of its fifty files -- whose question is then answered from partial context, silently.
    pub attached_files: usize,
    pub omitted_files: usize,
}

pub struct ProjectContext {
    root: PathBuf,
    instructions: Option<(String, String)>,
}

/// The `@` reference being edited at a byte-indexed caret position.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ActiveAtReference {
    /// Reference text without the leading `@` or surrounding quote.
    pub query: String,
    /// Byte range to replace with one value from `ProjectContext::at_path_candidates`.
    pub replacement: Range<usize>,
    /// The quote already used by the reference, if any.
    pub quote: Option<char>,
}

impl ProjectContext {
    pub fn discover() -> Result<Self> {
        Self::from_root(std::env::current_dir().context("could not determine working directory")?)
    }

    pub(crate) fn from_root(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to access {}", root.display()))?;
        let instructions = INSTRUCTION_FILES
            .iter()
            .find_map(|name| {
                let path = root.join(name);
                path.is_file().then_some((*name, path))
            })
            .map(|(name, path)| read_text_file(&path).map(|content| (name.to_string(), content)))
            .transpose()?;

        Ok(Self { root, instructions })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A stable identifier for this project, used to scope rows belonging to one project inside
    /// Kamui's single global database (`/index`'s `code_chunks`/`indexed_files`). The canonical
    /// root path: unique per checkout, and readable when inspecting the database by hand — unlike
    /// a hash, which would buy nothing here since the value is never shown to the model.
    pub fn key(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    pub fn instruction_name(&self) -> Option<&str> {
        self.instructions.as_ref().map(|(name, _)| name.as_str())
    }

    pub fn system_message(&self) -> Option<String> {
        self.instructions.as_ref().map(|(name, content)| {
            format!(
                "Follow the project instructions from {name} for this conversation:\n\n{content}"
            )
        })
    }

    /// Return insertion-ready `@` references for visible, non-ignored project files and
    /// directories. Paths use `/` on every platform; directories end in `/` and paths containing
    /// whitespace are double quoted. Named non-filesystem references are included as well.
    #[allow(dead_code)]
    pub fn at_path_candidates(&self) -> Result<Vec<String>> {
        let mut candidates = vec![
            "@clipboard".to_string(),
            "@diff".to_string(),
            "@staged".to_string(),
        ];
        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .build();

        for entry in walker {
            let entry = entry.with_context(|| {
                format!(
                    "could not enumerate project paths in {}",
                    self.root.display()
                )
            })?;
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if entry.depth() == 0 || file_type.is_symlink() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .expect("walked project entry is below its root");
            let mut reference = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if file_type.is_dir() {
                reference.push('/');
            }
            candidates.push(format_at_reference(&reference));
        }

        candidates.sort();
        Ok(candidates)
    }

    pub fn expand_file_references(&self, input: &str) -> Result<Expanded> {
        let references = file_references(input);
        if references.is_empty() {
            return Ok(Expanded {
                text: input.to_string(),
                ..Expanded::default()
            });
        }

        let mut total_bytes = 0;
        let mut context = String::new();
        let mut images = Vec::new();
        let mut attached_files = 0usize;
        let mut omitted_files = 0usize;
        for reference in references {
            // Named sources are not paths, so they are resolved before any filesystem lookup.
            let named = match reference.as_str() {
                "diff" => Some(("git diff".to_string(), self.read_git_diff(false)?)),
                "staged" => Some(("git diff --staged".to_string(), self.read_git_diff(true)?)),
                "clipboard" => match read_clipboard()? {
                    ClipboardContent::Text(text) => Some(("clipboard".to_string(), text)),
                    ClipboardContent::Image(image) => {
                        images.push(image);
                        context.push_str(
                            "\n\n<context source=\"clipboard\">(image attached)</context>",
                        );
                        continue;
                    }
                },
                _ => None,
            };
            if let Some((label, content)) = named {
                total_bytes += content.len();
                if total_bytes > MAX_CONTEXT_BYTES {
                    anyhow::bail!("attached context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
                }
                context.push_str(&format!(
                    "\n\n<context source=\"{label}\">\n{content}\n</context>"
                ));
                continue;
            }

            // Images cannot be inlined as text; they travel as attachments on the message. The
            // extension only selects this branch; the loader validates the actual magic bytes.
            if is_image_path(Path::new(&reference)) {
                images.push(read_project_image(&self.root, &reference)?.attachment);
                context.push_str(&format!(
                    "\n\n<context source=\"{reference}\">(image attached)</context>"
                ));
                continue;
            }

            let path = resolve_within_root(&self.root, &reference)?;
            if path.is_dir() {
                let budget = MAX_CONTEXT_BYTES.saturating_sub(total_bytes);
                let directory = read_project_directory(&self.root, &path, &reference, budget)?;
                total_bytes += directory.bytes;
                attached_files += directory.attached;
                omitted_files += directory.omitted;
                context.push_str(&directory.blocks);
                continue;
            }

            let content = read_text_file(&path)?;
            attached_files += 1;
            total_bytes += content.len();
            if total_bytes > MAX_CONTEXT_BYTES {
                anyhow::bail!("attached context exceeds {} KiB", MAX_CONTEXT_BYTES / 1024);
            }
            context.push_str(&format!(
                "\n\n<context source=\"{reference}\">\n{content}\n</context>"
            ));
        }

        Ok(Expanded {
            text: format!("{input}\n\nAttached project context:{context}"),
            images,
            attached_files,
            omitted_files,
        })
    }

    fn read_git_diff(&self, staged: bool) -> Result<String> {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .args(["diff", "--no-ext-diff", "--no-color"]);
        if staged {
            command.arg("--cached");
        }

        let output = command.output().context("failed to run git diff")?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("git diff failed: {error}");
        }
        let diff = String::from_utf8(output.stdout).context("git diff output is not UTF-8")?;
        Ok(if diff.is_empty() {
            "(no changes)".to_string()
        } else {
            diff
        })
    }
}

fn format_at_reference(reference: &str) -> String {
    if reference.chars().any(char::is_whitespace) {
        format!("@\"{reference}\"")
    } else {
        format!("@{reference}")
    }
}

/// Find the incomplete `@` reference containing the caret. The caret and returned replacement
/// range are UTF-8 byte offsets, matching Rust editor buffers and the expansion parser below.
#[allow(dead_code)]
pub fn active_at_reference(input: &str, caret: usize) -> Option<ActiveAtReference> {
    if caret > input.len() || !input.is_char_boundary(caret) {
        return None;
    }

    for (at, character) in input[..caret].char_indices().rev() {
        if character != '@' || !is_reference_boundary(input, at) {
            continue;
        }
        let start = at + 1;
        let next = input[start..].chars().next();
        if let Some(quote @ ('"' | '\'')) = next {
            let content_start = start + quote.len_utf8();
            if caret < content_start {
                continue;
            }
            let before_caret = &input[content_start..caret];
            if before_caret.contains(quote) {
                return None;
            }
            let replacement_end = input[caret..]
                .chars()
                .next()
                .filter(|character| *character == quote)
                .map_or(caret, |character| caret + character.len_utf8());
            return Some(ActiveAtReference {
                query: before_caret.to_string(),
                replacement: at..replacement_end,
                quote: Some(quote),
            });
        }

        let query = &input[start..caret];
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        return Some(ActiveAtReference {
            query: query.to_string(),
            replacement: at..caret,
            quote: None,
        });
    }
    None
}

/// Returns the valid attachment references in input, using a caller-provided candidate snapshot.
/// This intentionally does no filesystem work: the TUI refreshes the snapshot separately.
pub fn attachment_indicators(input: &str, candidates: &[String]) -> Vec<String> {
    let valid: HashSet<&str> = candidates
        .iter()
        .map(|candidate| candidate.trim_start_matches('@'))
        .collect();
    let mut seen = HashSet::new();
    file_references(input)
        .into_iter()
        .filter(|reference| valid.contains(reference.as_str()) && seen.insert(reference.clone()))
        .map(|reference| match reference.as_str() {
            "clipboard" => "clipboard".to_string(),
            "diff" => "diff".to_string(),
            "staged" => "staged".to_string(),
            _ if is_image_path(Path::new(&reference)) => reference,
            _ => reference,
        })
        .collect()
}

fn file_references(input: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut references = Vec::new();
    let mut index = 0;

    while index < input.len() {
        let Some(character) = input[index..].chars().next() else {
            break;
        };
        if character != '@' || !is_reference_boundary(input, index) {
            index += character.len_utf8();
            continue;
        }

        let after_at = index + character.len_utf8();
        let Some(next) = input[after_at..].chars().next() else {
            break;
        };

        let (reference, next_index) = if matches!(next, '"' | '\'') {
            quoted_reference(input, after_at, next)
        } else {
            unquoted_reference(input, after_at)
        };

        if !reference.is_empty() && seen.insert(reference.clone()) {
            references.push(reference);
        }
        index = next_index;
    }

    references
}

fn is_reference_boundary(input: &str, index: usize) -> bool {
    index == 0
        || input[..index]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn quoted_reference(input: &str, quote_index: usize, quote: char) -> (String, usize) {
    let start = quote_index + quote.len_utf8();
    let mut cursor = start;
    while cursor < input.len() {
        let character = input[cursor..]
            .chars()
            .next()
            .expect("cursor is on a character boundary");
        if character == quote {
            return (
                input[start..cursor].to_string(),
                cursor + character.len_utf8(),
            );
        }
        cursor += character.len_utf8();
    }

    // Leave malformed quoted references to the normal filesystem error path.
    unquoted_reference(input, quote_index)
}

fn unquoted_reference(input: &str, start: usize) -> (String, usize) {
    let mut end = start;
    while end < input.len() {
        let character = input[end..]
            .chars()
            .next()
            .expect("end is on a character boundary");
        if character.is_whitespace() {
            break;
        }
        end += character.len_utf8();
    }

    let reference = input[start..end]
        .trim_matches(|character: char| matches!(character, ',' | ';' | ':' | ')' | ']' | '}'));
    (reference.to_string(), end)
}

/// Resolve a project-relative reference to a real path inside the project root, rejecting absolute
/// paths and anything that escapes the root once symlinks are resolved. Shared by `@file` expansion
/// and the read-only tools so path safety lives in one place.
pub(crate) fn resolve_within_root(root: &Path, reference: &str) -> Result<PathBuf> {
    let relative = Path::new(reference);
    if relative.is_absolute() {
        anyhow::bail!("path must be relative to the project: {reference}");
    }

    let path = root.join(relative).canonicalize().with_context(|| {
        format!(
            "could not access {reference} relative to {}",
            root.display()
        )
    })?;
    if !path.starts_with(root) {
        anyhow::bail!("path is outside the project: {reference}");
    }

    Ok(path)
}

/// Resolve a project-relative path for writing. Unlike `resolve_within_root`, the file itself may
/// not exist yet, but its parent directory must already exist inside the project root.
pub fn resolve_for_write(root: &Path, reference: &str) -> Result<PathBuf> {
    let relative = Path::new(reference);
    if relative.is_absolute() {
        anyhow::bail!("path must be relative to the project: {reference}");
    }

    let joined = root.join(relative);
    let parent = joined
        .parent()
        .with_context(|| format!("path has no parent directory: {reference}"))?;
    let parent = parent.canonicalize().with_context(|| {
        format!(
            "the parent directory of {reference} does not exist in {}",
            root.display()
        )
    })?;
    if !parent.starts_with(root) {
        anyhow::bail!("path is outside the project: {reference}");
    }
    let name = joined
        .file_name()
        .with_context(|| format!("path has no file name: {reference}"))?;
    Ok(parent.join(name))
}

/// Read a UTF-8 text file identified by a project-relative reference.
pub fn read_project_file(root: &Path, reference: &str) -> Result<String> {
    let path = resolve_within_root(root, reference)?;
    if !path.is_file() {
        anyhow::bail!("path is not a file: {reference}");
    }

    read_text_file(&path)
}

/// List the entries of a project-relative directory. Directories are shown with a trailing slash and
/// sorted first; the `.git` directory is skipped to reduce noise. Use `.` for the project root.
pub fn list_project_directory(root: &Path, reference: &str) -> Result<String> {
    let path = resolve_within_root(root, reference)?;
    if !path.is_dir() {
        anyhow::bail!("path is not a directory: {reference}");
    }

    let mut entries: Vec<(bool, String)> = Vec::new();
    for entry in fs::read_dir(&path).with_context(|| format!("could not list {reference}"))? {
        let entry = entry.with_context(|| format!("could not read an entry in {reference}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        entries.push((is_dir, name));
    }
    if entries.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    // Directories first, then files, each alphabetically.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let listing = entries
        .into_iter()
        .map(|(is_dir, name)| if is_dir { format!("{name}/") } else { name })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(listing)
}

/// Attach the text files inside a project directory, honouring `.gitignore` and skipping hidden
/// files. Files are added in path order until the remaining context budget or the file cap runs
/// out; anything left over (too large, binary, or over budget) is reported rather than failing the
/// whole prompt. Returns the rendered context blocks and how many bytes they consumed.
/// What one `@dir` reference contributed: the context blocks, the bytes they cost, and how many
/// files were taken versus left out.
#[derive(Default)]
struct DirectoryAttachment {
    blocks: String,
    bytes: usize,
    attached: usize,
    omitted: usize,
}

fn read_project_directory(
    root: &Path,
    directory: &Path,
    reference: &str,
    budget: usize,
) -> Result<DirectoryAttachment> {
    let mut paths: Vec<PathBuf> = ignore::WalkBuilder::new(directory)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Honour .gitignore even when the project is not (yet) a git repository.
        .require_git(false)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .collect();
    paths.sort();

    let mut blocks = String::new();
    let mut used = 0;
    let mut attached = 0;
    let mut omitted = 0;
    for path in paths {
        if attached >= MAX_DIRECTORY_FILES {
            omitted += 1;
            continue;
        }
        // Binary or oversized files are skipped, not fatal.
        let Ok(content) = read_text_file(&path) else {
            omitted += 1;
            continue;
        };
        if used + content.len() > budget {
            omitted += 1;
            continue;
        }
        used += content.len();
        attached += 1;
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        blocks.push_str(&format!(
            "\n\n<context source=\"{label}\">\n{content}\n</context>"
        ));
    }

    if attached == 0 && omitted == 0 {
        anyhow::bail!("no attachable text files found in {reference}");
    }
    if omitted > 0 {
        blocks.push_str(&format!(
            "\n\n<context source=\"{reference}\">({omitted} more files omitted: binary, too large, or over the context budget)</context>"
        ));
    }
    Ok(DirectoryAttachment {
        blocks,
        bytes: used,
        attached,
        omitted,
    })
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

pub fn is_image_reference(reference: &str) -> bool {
    is_image_path(Path::new(reference))
}

/// A decoded project image ready for native multimodal transport.
#[derive(Debug)]
pub struct ProjectImage {
    pub attachment: ImageAttachment,
    pub filename: String,
    pub media_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub resized: bool,
}

impl ProjectImage {
    pub fn metadata(&self) -> String {
        format!(
            "Image: {}\nMIME type: {}\nDimensions: {}x{}\nOriginal dimensions: {}x{}\nFile size: {} bytes\nResized: {}",
            self.filename,
            self.media_type,
            self.width,
            self.height,
            self.original_width,
            self.original_height,
            self.file_size,
            self.resized
        )
    }
}

/// Decode an image selected by the model, validating its real format from the bytes. Images that
/// exceed provider-friendly dimensions are resized while preserving aspect ratio and encoded PNG.
pub fn read_project_image(root: &Path, reference: &str) -> Result<ProjectImage> {
    use image::ImageFormat;

    let path = resolve_within_root(root, reference)?;
    if !path.is_file() {
        anyhow::bail!("path is not a file: {reference}");
    }
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "{reference} exceeds the {} MiB image limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let format = image::guess_format(&bytes)
        .with_context(|| format!("{reference} is not a supported PNG, JPEG, WebP, or GIF image"))?;
    let media_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Gif => "image/gif",
        _ => anyhow::bail!("{reference} is not a supported PNG, JPEG, WebP, or GIF image"),
    };
    let decoded = image::load_from_memory_with_format(&bytes, format)
        .with_context(|| format!("failed to decode image {reference}"))?;
    let original_width = decoded.width();
    let original_height = decoded.height();
    let pixels = u64::from(original_width) * u64::from(original_height);
    let resize_scale = (MAX_IMAGE_DIMENSION as f64
        / f64::from(original_width.max(original_height)))
    .min((MAX_IMAGE_PIXELS as f64 / pixels.max(1) as f64).sqrt())
    .min(1.0);
    let resized = resize_scale < 1.0;
    let decoded = if resized {
        let width = (f64::from(original_width) * resize_scale).floor().max(1.0) as u32;
        let height = (f64::from(original_height) * resize_scale).floor().max(1.0) as u32;
        decoded.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
    } else {
        decoded
    };
    let width = decoded.width();
    let height = decoded.height();
    let (output_type, output) = if resized {
        let mut output = std::io::Cursor::new(Vec::new());
        decoded
            .write_to(&mut output, ImageFormat::Png)
            .with_context(|| format!("failed to encode resized image {reference}"))?;
        ("image/png", output.into_inner())
    } else {
        (media_type, bytes)
    };
    Ok(ProjectImage {
        attachment: ImageAttachment {
            media_type: output_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(output),
        },
        filename: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        media_type: media_type.to_string(),
        original_width,
        original_height,
        width,
        height,
        file_size: metadata.len(),
        resized,
    })
}

/// What the clipboard held: text if any, otherwise a pasted image (e.g. a screenshot).
enum ClipboardContent {
    Text(String),
    Image(ImageAttachment),
}

/// Read the operating system clipboard, preferring text and falling back to image data so a
/// screenshot can be pasted directly. Errors clearly when the clipboard is unavailable (e.g. a
/// headless session) or holds neither.
fn read_clipboard() -> Result<ClipboardContent> {
    let mut clipboard =
        arboard::Clipboard::new().context("could not access the system clipboard")?;

    if let Ok(text) = clipboard.get_text()
        && !text.trim().is_empty()
    {
        return Ok(ClipboardContent::Text(text));
    }

    match clipboard.get_image() {
        Ok(image) => Ok(ClipboardContent::Image(encode_png(
            image.width,
            image.height,
            &image.bytes,
        )?)),
        Err(_) => anyhow::bail!("the clipboard holds no text or image"),
    }
}

/// Encode raw RGBA pixels as a PNG image attachment.
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<ImageAttachment> {
    let width = u32::try_from(width).context("clipboard image is too wide")?;
    let height = u32::try_from(height).context("clipboard image is too tall")?;

    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("failed to encode the clipboard image")?;
        writer
            .write_image_data(rgba)
            .context("failed to encode the clipboard image")?;
    }
    if png.len() as u64 > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "the clipboard image exceeds the {} MiB image limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }

    Ok(ImageAttachment {
        media_type: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

fn read_text_file(path: &Path) -> Result<String> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        anyhow::bail!("{} exceeds the 1 MiB file limit", path.display());
    }
    fs::read_to_string(path)
        .with_context(|| format!("{} is not a readable UTF-8 text file", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn a_directory_reports_what_it_could_not_attach() {
        // The omission note already went to the model; these counts are what lets the person who
        // typed `@dir` be told as well.
        let root = project();
        let dir = root.join("many");
        fs::create_dir(&dir).unwrap();
        for i in 0..60 {
            fs::write(dir.join(format!("file-{i:02}.txt")), "x").unwrap();
        }
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let expanded = context.expand_file_references("look at @many").unwrap();

        // MAX_DIRECTORY_FILES caps the attachment; the rest are counted, not dropped in silence.
        assert_eq!(expanded.attached_files, MAX_DIRECTORY_FILES);
        assert_eq!(expanded.omitted_files, 60 - MAX_DIRECTORY_FILES);
        assert!(
            expanded.text.contains("more files omitted"),
            "the model is still told too"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_single_file_reference_counts_as_one_attachment() {
        let root = project();
        fs::write(root.join("notes.txt"), "hello").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let expanded = context.expand_file_references("see @notes.txt").unwrap();

        assert_eq!(expanded.attached_files, 1);
        assert_eq!(expanded.omitted_files, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_prompt_without_references_counts_nothing() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();
        let expanded = context.expand_file_references("just a question").unwrap();
        assert_eq!(expanded.attached_files, 0);
        assert_eq!(expanded.omitted_files, 0);
        assert_eq!(expanded.text, "just a question");
        fs::remove_dir_all(root).unwrap();
    }

    fn project() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-context-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn loads_kamui_instructions_before_agents() {
        let root = project();
        fs::write(root.join("KAMUI.md"), "Use Rust.").unwrap();
        fs::write(root.join("AGENTS.md"), "Use Go.").unwrap();

        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.instruction_name(), Some("KAMUI.md"));
        assert!(context.system_message().unwrap().contains("Use Rust."));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_each_file_reference_once() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context
            .expand_file_references("Explain @src/main.rs and @src/main.rs")
            .unwrap();

        assert_eq!(prompt.text.matches("<context source=").count(), 1);
        assert!(prompt.text.contains("fn main() {}"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_quoted_file_references_with_spaces() {
        let root = project();
        fs::create_dir(root.join("docs")).unwrap();
        fs::write(root.join("docs/My Notes.md"), "remember this").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context
            .expand_file_references(r#"Explain @"docs/My Notes.md""#)
            .unwrap();

        assert!(
            prompt
                .text
                .contains("<context source=\"docs/My Notes.md\">")
        );
        assert!(prompt.text.contains("remember this"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_staged_git_diff() {
        let root = project();
        fs::write(root.join("file.txt"), "hello\n").unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["add", "file.txt"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context.expand_file_references("Review @staged").unwrap();

        assert!(
            prompt
                .text
                .contains("<context source=\"git diff --staged\">")
        );
        assert!(prompt.text.contains("+hello"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_prompts_without_references_unchanged() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(
            context.expand_file_references("hello").unwrap().text,
            "hello"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_absolute_paths() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();
        let prompt = format!("Read @{}", root.join("main.rs").display());

        let error = context.expand_file_references(&prompt).unwrap_err();
        assert!(error.to_string().contains("relative"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attaches_a_directory_and_honours_ignore_rules() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/keep.rs"), "fn keep() {}").unwrap();
        fs::write(root.join("src/skip.rs"), "fn skip() {}").unwrap();
        fs::write(root.join("src/.gitignore"), "skip.rs\n").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let expanded = context.expand_file_references("Review @src").unwrap();

        assert!(expanded.text.contains("fn keep() {}"));
        // The ignored file and the hidden .gitignore itself are left out.
        assert!(!expanded.text.contains("fn skip() {}"));
        assert!(!expanded.text.contains(".gitignore"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_missing_files() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert!(context.expand_file_references("Read @nope.rs").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_files_over_the_size_limit() {
        let root = project();
        fs::write(root.join("big.txt"), vec![b'a'; 1024 * 1024 + 1]).unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let error = context.expand_file_references("Read @big.txt").unwrap_err();
        assert!(error.to_string().contains("1 MiB"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_unstaged_git_diff() {
        let root = project();
        fs::write(root.join("file.txt"), "hello\n").unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["add", "file.txt"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        // Modify the tracked file so it differs from the index without a commit.
        fs::write(root.join("file.txt"), "hello\nworld\n").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let prompt = context.expand_file_references("Review @diff").unwrap();

        assert!(prompt.text.contains("<context source=\"git diff\">"));
        assert!(prompt.text.contains("+world"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn falls_back_to_agents_when_kamui_absent() {
        let root = project();
        fs::write(root.join("AGENTS.md"), "Use Go.").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.instruction_name(), Some("AGENTS.md"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_no_instructions_when_none_present() {
        let root = project();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        assert_eq!(context.instruction_name(), None);
        assert!(context.system_message().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attaches_an_image_reference_as_an_attachment() {
        let root = project();
        let attachment = encode_png(1, 1, &[255, 0, 0, 255]).unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(attachment.data)
            .unwrap();
        fs::write(root.join("shot.png"), png).unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let expanded = context.expand_file_references("look at @shot.png").unwrap();

        assert_eq!(expanded.images.len(), 1);
        assert_eq!(expanded.images[0].media_type, "image/png");
        assert!(!expanded.images[0].data.is_empty());
        // The text notes the attachment but does not inline the bytes.
        assert!(expanded.text.contains("shot.png"));
        assert!(!expanded.text.contains("iVBOR"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_images_over_the_size_limit() {
        let root = project();
        fs::write(
            root.join("big.jpg"),
            vec![0u8; (MAX_IMAGE_BYTES + 1) as usize],
        )
        .unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let error = context
            .expand_file_references("see @big.jpg")
            .unwrap_err()
            .to_string();

        assert!(error.contains("image limit"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn encodes_clipboard_pixels_into_a_png_attachment() {
        // A single opaque red pixel.
        let attachment = encode_png(1, 1, &[255, 0, 0, 255]).unwrap();

        assert_eq!(attachment.media_type, "image/png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(attachment.data)
            .unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n"); // PNG magic number
    }

    #[test]
    fn file_references_strip_punctuation_and_deduplicate() {
        let refs = file_references("see @a.rs, and @b.rs; also @a.rs and a bare @");
        assert_eq!(refs, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn file_references_support_quoted_paths_with_spaces() {
        let refs = file_references(r#"see @"docs/My Notes.md" and @'src/other file.rs'."#);
        assert_eq!(
            refs,
            vec![
                "docs/My Notes.md".to_string(),
                "src/other file.rs".to_string()
            ]
        );
    }

    #[test]
    fn file_references_ignore_mid_word_at_signs() {
        let refs = file_references("email a@b.test then read @src/main.rs");
        assert_eq!(refs, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn attachment_indicators_validate_snapshot_and_deduplicate() {
        let candidates = vec![
            "@src/".to_string(),
            "@src/main.rs".to_string(),
            "@shot.png".to_string(),
            "@diff".to_string(),
            "@staged".to_string(),
            "@clipboard".to_string(),
        ];
        assert_eq!(
            attachment_indicators(
                "@src/ @shot.png @diff @staged @clipboard @diff nope@src/ @missing",
                &candidates
            ),
            ["src/", "shot.png", "diff", "staged", "clipboard"]
        );
    }

    #[test]
    fn path_candidates_are_ignore_aware_portable_and_insertion_ready() {
        let root = project();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir(root.join("My Notes")).unwrap();
        fs::create_dir(root.join(".hidden")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/ignored.rs"), "ignored").unwrap();
        fs::write(root.join("My Notes/todo.txt"), "todo").unwrap();
        fs::write(root.join(".hidden/secret.txt"), "secret").unwrap();
        fs::write(root.join(".gitignore"), "src/ignored.rs\n").unwrap();
        let context = ProjectContext::from_root(root.clone()).unwrap();

        let candidates = context.at_path_candidates().unwrap();

        assert!(candidates.contains(&"@diff".to_string()));
        assert!(candidates.contains(&"@staged".to_string()));
        assert!(candidates.contains(&"@clipboard".to_string()));
        assert!(candidates.contains(&"@src/".to_string()));
        assert!(candidates.contains(&"@src/nested/".to_string()));
        assert!(candidates.contains(&"@src/main.rs".to_string()));
        assert!(candidates.contains(&r#"@"My Notes/""#.to_string()));
        assert!(candidates.contains(&r#"@"My Notes/todo.txt""#.to_string()));
        assert!(!candidates.iter().any(|value| value.contains("ignored")));
        assert!(!candidates.iter().any(|value| value.contains("hidden")));
        assert!(!candidates.iter().any(|value| value.contains("gitignore")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_reference_matches_boundaries_and_tracks_replacement() {
        assert_eq!(
            active_at_reference("review @src/ma", 14),
            Some(ActiveAtReference {
                query: "src/ma".to_string(),
                replacement: 7..14,
                quote: None,
            })
        );
        assert_eq!(active_at_reference("email a@src", 11), None);
        assert_eq!(active_at_reference("done @src now", 13), None);
    }

    #[test]
    fn active_quoted_reference_allows_spaces_and_consumes_closing_quote() {
        let input = r#"see @"My Notes/ma" later"#;
        assert_eq!(
            active_at_reference(input, 17),
            Some(ActiveAtReference {
                query: "My Notes/ma".to_string(),
                replacement: 4..18,
                quote: Some('"'),
            })
        );
        assert_eq!(active_at_reference(input, 18), None);
        assert_eq!(
            active_at_reference("see @'My Notes", 14).unwrap().quote,
            Some('\'')
        );
    }

    #[test]
    fn lists_directory_entries_within_the_project() {
        let root = project();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("a.txt"), "x").unwrap();
        let canonical = root.canonicalize().unwrap();

        let listing = list_project_directory(&canonical, ".").unwrap();

        assert!(listing.contains("src/"));
        assert!(listing.contains("a.txt"));
        assert!(listing.find("src/").unwrap() < listing.find("a.txt").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_rejects_a_file_path() {
        let root = project();
        fs::write(root.join("a.txt"), "x").unwrap();
        let canonical = root.canonicalize().unwrap();

        let error = list_project_directory(&canonical, "a.txt").unwrap_err();
        assert!(error.to_string().contains("not a directory"));
        fs::remove_dir_all(root).unwrap();
    }
}
