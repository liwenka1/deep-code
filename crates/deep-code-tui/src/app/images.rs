//! Attached images in the composer.
//!
//! An attachment is a *label on text*, not a second input stream. The chip
//! (`[图片 #2 PNG]`) is inserted into `input` as ordinary characters and the
//! real path is kept beside it here, which buys three things at once:
//!
//! * the chip's position in the text is the image's position in the message, so
//!   "this one" keeps meaning the same picture without inventing a parts model;
//! * the chip is sent as literal text, so the model can refer to "#2" when the
//!   user attached several;
//! * `expand_pasted` and everything else that treats the composer as a string
//!   keeps working untouched.
//!
//! The attachment list is the *source of truth*; the chip is only how it is
//! addressed. So a chip copied and pasted twice still means one image, and a
//! chip edited by hand still means the image it was created for. What it does
//! not allow is an attachment no longer visible in the draft, and
//! [`App::sync_images`] is what enforces that.
//!
//! One consequence worth naming: the chip is *also* the only model-facing string
//! in this crate taken from the UI language pack rather than written bilingual.
//! That is deliberate and unlike the paste chip, which is expanded back to its
//! content before sending and never reaches the provider at all. This chip does
//! reach it, but it sits inline in a sentence the user is writing in their own
//! language — a bilingual `[图片 #1 PNG / image #1 PNG]` would be the odd one out
//! in an otherwise all-Chinese or all-English prompt.

use super::*;

use deep_code_agent::{ImageError, ImageFormat, MAX_IMAGES_PER_MESSAGE, MAX_TOTAL_BYTES, inspect};

/// One image attached to the draft, plus the chip standing in for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachedImage {
    /// The chip's number, kept apart from the chip string so `/image` can list
    /// what `#2` refers to without re-parsing its own label.
    pub(crate) id: usize,
    /// The literal text in `input` that addresses this image. Kept so a
    /// recalled prompt restores its chips *and* their numbers, and so a
    /// deletion can be matched back to the attachment it removes.
    pub(crate) chip: String,
    pub(crate) path: PathBuf,
    pub(crate) format: ImageFormat,
    pub(crate) bytes: u64,
}

impl AttachedImage {
    /// Human-readable size for the status line and `/image`.
    pub(crate) fn size_label(&self) -> String {
        format_size(self.bytes)
    }
}

/// A byte count as a short label. Powers of 1024, matching how the caps in
/// `image.rs` are written (32 MiB, 64 MiB).
pub(crate) fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Whether one line's trailing name *claims* to be an image.
///
/// The extension is the whole test, and it is what keeps a failure from being
/// noise: a path to a `.txt` in a pasted list is prose — a log excerpt, a `find`
/// output — while a path to a `.png` that did not attach is the thing the user
/// was trying to do. Only the second deserves a status line.
fn looks_like_image_name(line: &str) -> bool {
    const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
    let Some(name) = unescape_path(line)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
    else {
        return false;
    };
    IMAGE_EXTENSIONS
        .iter()
        .any(|extension| name.ends_with(&format!(".{extension}")))
}

/// What a paste turned out to be.
///
/// Three outcomes rather than two, because "this is not a path" and "this is a
/// path I refuse" need different words — and collapsing them into one `None` was
/// why a dragged file too large to attach landed as unexplained text.
enum PastedContent {
    /// Every line named a usable image.
    Images(Vec<PathBuf>),
    /// A line named a file that exists, is named like an image, and cannot be
    /// attached. Carries the reason.
    Unusable { path: PathBuf, error: ImageError },
    /// Ordinary text: not a paste of image paths at all.
    Text,
}

impl App {
    /// Attach `path` to the draft, inserting its chip at the cursor.
    ///
    /// Returns whether it was attached. A refusal leaves the draft alone, which
    /// is the whole point of checking here rather than letting the turn fail
    /// later: the user keeps their text and their images and can just try again.
    pub(crate) fn attach_image(&mut self, path: PathBuf) -> bool {
        if self.at_image_capacity() {
            return false;
        }
        let attachment = match inspect(&path) {
            Ok(attachment) => attachment,
            Err(error) => {
                // The reason comes from the image layer, which is where the
                // four ways a file can fail to be an image are known.
                self.status = self.tr_with(
                    TextId::ImageAttachFailed,
                    &[("reason", &self.image_error_reason(&error))],
                );
                return false;
            }
        };
        // The request-wide budget, checked against the draft's own total. The
        // per-image cap times the per-message cap is twice what one request may
        // carry, so images that each pass on their own can still be a request
        // that cannot hold them — and the request-time fallback only tells the
        // *model*. Here the picture is still in the user's hand.
        if !self.has_room_for(attachment.bytes) {
            self.status = self.tr_with(
                TextId::ImageDraftTooLarge,
                &[("limit", &format_size(MAX_TOTAL_BYTES))],
            );
            return false;
        }
        self.next_image_id += 1;
        let id = self.next_image_id;
        let chip = self.tr_with(
            TextId::ImageChip,
            &[
                ("id", &id.to_string()),
                ("format", attachment.format.label()),
            ],
        );
        self.insert_str_at_cursor(&chip);
        self.attached_images.push(AttachedImage {
            id,
            chip,
            path,
            format: attachment.format,
            bytes: attachment.bytes,
        });
        let attached = self.attached_images.last().expect("just pushed");
        self.status = self.tr_with(
            TextId::ImageAttached,
            &[
                ("id", &id.to_string()),
                ("format", attached.format.label()),
                ("size", &attached.size_label()),
            ],
        );
        true
    }

    /// `/image` — list what is attached, or attach a path.
    ///
    /// The chips already show *that* something is attached and where it sits in
    /// the draft; this adds the two things a chip cannot carry, which are the
    /// path and the size. That is why it is a listing rather than a live
    /// indicator: the information is on screen, and what is missing is the part
    /// that only matters when something looks wrong.
    pub(crate) fn handle_image_command(&mut self, arg: &str) {
        if arg.is_empty() {
            if self.attached_images.is_empty() {
                self.status = self.tr(TextId::ImageListEmpty).to_string();
                return;
            }
            let listing: Vec<String> = self
                .attached_images
                .iter()
                .map(|image| {
                    self.tr_with(
                        TextId::ImageListEntry,
                        &[
                            ("id", &image.id.to_string()),
                            ("format", image.format.label()),
                            ("size", &image.size_label()),
                            ("path", &image.path.display().to_string()),
                        ],
                    )
                })
                .collect();
            self.history.push(HistoryCell::system(listing.join("\n")));
            return;
        }
        // A relative path is relative to the workspace, not to wherever the
        // process happens to have been started. Quoted and escaped spellings are
        // accepted here too, so `/image '/tmp/my shot.png'` works like a dropped
        // file does.
        let path = self.resolve_image_path(&unescape_path(arg));
        self.attach_image(path);
    }

    /// A user-facing reason for a failed attachment.
    ///
    /// Deliberately not [`ImageError::note`]: that one is written for the model
    /// and lands in the context window, this one is a status line.
    pub(crate) fn image_error_reason(&self, error: &ImageError) -> String {
        match error {
            ImageError::Missing => self.tr(TextId::ImageErrorMissing).to_string(),
            ImageError::NotAFile => self.tr(TextId::ImageErrorNotAFile).to_string(),
            ImageError::Unreadable(reason) => {
                self.tr_with(TextId::ImageErrorUnreadable, &[("reason", reason)])
            }
            ImageError::TooLarge { bytes } => self.tr_with(
                TextId::ImageErrorTooLarge,
                &[("size", &format_size(*bytes))],
            ),
            ImageError::UnsupportedFormat => self.tr(TextId::ImageErrorUnsupported).to_string(),
        }
    }

    /// Drop every attachment whose chip is no longer in the draft.
    ///
    /// Runs after any edit. Deleting a chip by any route — `Ctrl+W` cutting
    /// through the middle of one, `Ctrl+U` taking the line, a drag-selection
    /// overwrite — has to take the attachment with it, or the user sends a
    /// picture they can no longer see. Matching on the chip rather than on a
    /// cursor offset is what makes that true for all of those at once.
    pub(crate) fn sync_images(&mut self) {
        if self.attached_images.is_empty() {
            return;
        }
        self.attached_images
            .retain(|image| self.input.contains(image.chip.as_str()));
    }

    /// The attachment chip that ends exactly at the cursor.
    ///
    /// Used to delete one as a unit: backspacing through `[图片 #1 PNG]` a
    /// character at a time is never what someone means, and the alternative is
    /// a half-eaten chip that `sync_images` then throws away.
    pub(crate) fn chip_ending_at_cursor(&self) -> Option<usize> {
        let byte = byte_idx(
            self.input.as_str(),
            self.input_cursor.min(char_count(&self.input)),
        );
        let prefix = &self.input[..byte];
        self.attached_images
            .iter()
            .position(|image| prefix.ends_with(image.chip.as_str()))
    }

    /// The attachment chip that starts exactly at the cursor, for Delete.
    pub(crate) fn chip_starting_at_cursor(&self) -> Option<usize> {
        let byte = byte_idx(
            self.input.as_str(),
            self.input_cursor.min(char_count(&self.input)),
        );
        let tail = &self.input[byte..];
        self.attached_images
            .iter()
            .position(|image| tail.starts_with(image.chip.as_str()))
    }

    /// The images to send, in the order they appear in the draft.
    ///
    /// Ordered by chip position rather than by attachment order: the chips are
    /// what the model reads, so the parts array has to line up with what the
    /// text says about "#1" and "#2".
    ///
    /// Filtered, not merely ordered. `sync_images` already drops an attachment
    /// whose chip left the draft, so this filter should never remove anything —
    /// and that is precisely why it is here: the alternative was a
    /// `unwrap_or(usize::MAX)` that silently *kept* such an attachment and sent
    /// it, making this function the one place the documented invariant was not
    /// enforced.
    pub(crate) fn attached_image_paths(&self) -> Vec<PathBuf> {
        let mut located: Vec<(usize, &AttachedImage)> = self
            .attached_images
            .iter()
            .filter_map(|image| {
                self.input
                    .find(image.chip.as_str())
                    .map(|position| (position, image))
            })
            .collect();
        located.sort_by_key(|(position, _)| *position);
        located
            .into_iter()
            .map(|(_, image)| image.path.clone())
            .collect()
    }

    /// The name of an image file a single-line paste mentioned but we could not
    /// open.
    ///
    /// Exists to end one specific confusion: copying a file in Finder puts both
    /// the file *and its name* on the pasteboard, and a terminal's own paste key
    /// picks the name — so the user sees `shot.png` appear in the composer and
    /// concludes the feature is broken, when what they wanted was the key that
    /// makes us read the pasteboard ourselves. The text still goes in as text;
    /// this only supplies the reason it was not attached.
    ///
    /// Deliberately narrow: one line, and a name that claims to be an image. A
    /// paste that merely contains a `.png` word in a sentence must not be
    /// second-guessed, which is why `lines().count() == 1` and a bare filename
    /// are both required.
    pub(crate) fn unresolved_image_name(text: &str) -> Option<String> {
        let line = text.trim();
        if line.is_empty() || line.contains('\n') || !looks_like_image_name(line) {
            return None;
        }
        Some(
            unescape_path(line)
                .file_name()?
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Attach whatever a paste named, if it named images.
    ///
    /// Returns whether the paste was *about* an image — attached, or refused with
    /// the reason — and so must not go in as text. Every image entry point that
    /// arrives as text funnels through here, which is also why the clipboard
    /// fallback lives at the bottom rather than in one caller.
    pub(crate) fn attach_pasted_images(&mut self, text: &str) -> bool {
        match self.classify_paste(text) {
            PastedContent::Images(paths) => {
                for path in paths {
                    // One refusal is enough — the cap or a bad file will refuse
                    // the rest too, and the status line already says why.
                    if !self.attach_image(path) {
                        break;
                    }
                }
                true
            }
            PastedContent::Unusable { path, error } => {
                // Named and explained, then the text still goes in: the user may
                // have meant the path as a reference, and swallowing their paste
                // would be worse than an unexplained one.
                let reason = self.image_error_reason(&error);
                self.status = self.tr_with(
                    TextId::ImageAttachFailedFor,
                    &[("path", &path.display().to_string()), ("reason", &reason)],
                );
                false
            }
            PastedContent::Text => {
                // A lone image *name*, which is what a terminal's own paste key
                // produces for a copied file: it can only hand over the
                // pasteboard's text, and that text is the file's name. The file
                // itself may still be on the pasteboard — see
                // `attach_clipboard_image_quietly`.
                if let Some(name) = Self::unresolved_image_name(text)
                    && !self.attach_clipboard_image_quietly()
                {
                    self.status = self
                        .tr_with(TextId::ImageNamePasted, &[("name", name.as_str())])
                        .to_string();
                }
                false
            }
        }
    }

    /// Sort a paste into images, a refusal, or text.
    ///
    /// All-or-nothing on purpose: a paste that merely *mentions* a path — a log
    /// excerpt, a diff — stays text, which is what `@` and `/image` are for. A
    /// line that names nothing is what makes a paste text; a line that names a
    /// file we refuse is reported as such, because that is the user's own path
    /// pointing at something we said no to.
    ///
    /// Relative names resolve against the workspace like every other image entry
    /// point. A drag always hands over an absolute path, so this only matters for
    /// a hand-pasted `logo.png` — and there it is the difference between attaching
    /// the file in the project and attaching whichever same-named file the process
    /// happens to be sitting next to.
    fn classify_paste(&self, text: &str) -> PastedContent {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            return PastedContent::Text;
        }
        let mut paths = Vec::with_capacity(lines.len());
        for line in lines {
            let path = self.resolve_image_path(&unescape_path(line));
            match inspect(&path) {
                Ok(_) => paths.push(path),
                // Nothing is there: this was never a path to a file.
                Err(ImageError::Missing | ImageError::NotAFile) => return PastedContent::Text,
                // Something is there that we will not send. Only worth saying
                // when the name claims to be an image — otherwise this is a list
                // of paths in someone's prose, and an error line would be noise.
                Err(error) if looks_like_image_name(line) => {
                    return PastedContent::Unusable { path, error };
                }
                Err(_) => return PastedContent::Text,
            }
        }
        PastedContent::Images(paths)
    }

    /// Attach the image on the system clipboard, if there is one.
    ///
    /// Called on `Ctrl+V`. A terminal that handles the paste itself never sends
    /// us this key — the text-paste path is what covers that case — and the one
    /// that does is asking us to read the clipboard, which is the only way to get
    /// an *image* that the pasteboard carries without any text: a screenshot, or
    /// a picture copied out of another application.
    ///
    /// Reports finding nothing, unlike the quiet path below. A `Ctrl+V` that
    /// reached us is a deliberate request, so silence would leave the user unable
    /// to tell "we looked and the clipboard was empty" from "the terminal ate the
    /// key" — which is the difference between a bug here and a limitation of the
    /// terminal, and the one thing a report of "nothing happens" cannot say on
    /// its own.
    pub(crate) fn paste_clipboard_image(&mut self) {
        let Some(read) = crate::clipboard::read_image() else {
            self.status = self.tr(TextId::ImageClipboardEmpty).to_string();
            return;
        };
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(reason) => {
                self.status = self.tr_with(TextId::ImageAttachFailed, &[("reason", &reason)]);
                return;
            }
        };
        // Refused *before* it is stored. `attach_image` checks the same cap, but
        // by then the bytes would already be a file under
        // `<workspace>/.deep-code/images/` — and nothing sweeps that directory,
        // so a rejected paste would be permanent litter.
        if self.at_image_capacity() {
            return;
        }
        self.attach_stored_image(&bytes);
    }

    /// The clipboard's image bytes, when there are any to be had.
    ///
    /// A blunter question than [`crate::clipboard::read_image`] answers, because
    /// the callers below have no interest in *why* there was nothing: they are
    /// second chances, not requests the user made.
    fn clipboard_image_bytes() -> Option<Vec<u8>> {
        crate::clipboard::read_image()?.ok()
    }

    /// Whether one more image of `bytes` fits the request budget the draft has
    /// left.
    ///
    /// The per-image cap times the per-message cap is twice what one request may
    /// carry, so images that each pass on their own can still be a request that
    /// cannot hold them — and the request-time fallback only tells the *model*.
    /// `attach_image` asks this for every path-shaped entry point; the clipboard
    /// paths ask it too, before writing, because they store first and attach
    /// second.
    fn has_room_for(&self, bytes: u64) -> bool {
        let attached: u64 = self.attached_images.iter().map(|image| image.bytes).sum();
        attached.saturating_add(bytes) <= MAX_TOTAL_BYTES
    }

    /// Store image bytes under the workspace and attach them.
    ///
    /// The budget is asked *before* the write, not only inside `attach_image`:
    /// nothing sweeps `.deep-code/images/`, so a paste refused after it was
    /// stored would be permanent litter — the same thing the count check above
    /// exists to avoid. A storage failure, or a refusal here, is reported by
    /// either caller: it means the image was found and could not be kept, which
    /// is worth saying wherever it happens.
    fn attach_stored_image(&mut self, bytes: &[u8]) -> bool {
        if !self.has_room_for(bytes.len() as u64) {
            self.status = self.tr_with(
                TextId::ImageDraftTooLarge,
                &[("limit", &format_size(MAX_TOTAL_BYTES))],
            );
            return false;
        }
        match deep_code_agent::store(&self.workspace, bytes) {
            Ok(path) => self.attach_image(path),
            Err(error) => {
                self.status =
                    self.tr_with(TextId::ImageAttachFailed, &[("reason", &error.to_string())]);
                false
            }
        }
    }

    /// Attach the clipboard's image, saying nothing when there is none.
    ///
    /// Two callers, both *quiet* — neither is a request the user made of us:
    ///
    /// * A text paste that was exactly a lone image name we could not open. This
    ///   is what makes the terminal's **own** paste key work for images: `Cmd+V`
    ///   is consumed by the terminal, which can only hand over the pasteboard's
    ///   text, and for a file copied in Finder that text is the file's name. What
    ///   makes asking the clipboard the right answer rather than a guess: a Finder
    ///   copy puts the file *and* the name on the pasteboard together, while
    ///   copying text replaces the pasteboard outright — so "the clipboard holds
    ///   an image" and "the user pasted a lone image name" cannot both be true by
    ///   accident.
    /// * An **empty** paste. A pasteboard carrying pixels and no text at all — a
    ///   screenshot — leaves some terminals with nothing to send; those that send
    ///   the empty paste anyway hand us the one signal that something was
    ///   requested, and there is nothing else an empty paste could mean.
    ///
    /// Returns whether anything was attached; a caller that gets `false` falls
    /// through and lets the paste be whatever it was.
    pub(crate) fn attach_clipboard_image_quietly(&mut self) -> bool {
        let Some(bytes) = Self::clipboard_image_bytes() else {
            return false;
        };
        if self.at_image_capacity() {
            return false;
        }
        self.attach_stored_image(&bytes)
    }

    /// Attach the image an `@`-reference names, in place of the reference.
    ///
    /// The `@` menu inserts a path because that is what the string means
    /// everywhere else; when it names an image, the chip replaces it rather than
    /// leaving the user to submit a path the model cannot open. Returns whether
    /// the reference was converted.
    pub(crate) fn attach_if_image_path(&mut self, path: &std::path::Path) -> bool {
        let path = self.resolve_image_path(path);
        if inspect(&path).is_err() {
            return false;
        }
        // Take back the trailing token the menu was completing — it always runs
        // to the end of the draft, since it is what the cursor is sitting in.
        //
        // `trailing_token_start` returns a **byte** index, and every other index
        // in this file is a **char** index: `input_cursor`, `char_count`, and
        // `drain_chars` (whose `chars.drain(start..end)` counts chars). Passing
        // it through unconverted cut the wrong range the moment anything
        // multi-byte preceded the token — `@shot` behind a `图 ` became `@s` and
        // the leftover went to the model as ordinary text. ASCII prefixes hid it,
        // because there the two indices coincide.
        let token_start = self.input[..self.trailing_token_start()].chars().count();
        let end = char_count(&self.input);
        self.drain_chars(token_start, end);
        self.input_cursor = token_start;
        self.attach_image(path)
    }

    /// A user-named image path, resolved against the workspace when relative.
    ///
    /// The `@` menu's entries are workspace-relative (`App::workspace_files`,
    /// filled by `list_workspace_files`), so resolving them against the process's
    /// working directory works only while the two happen to coincide — they stop
    /// coinciding as soon as a session is resumed for a different root.
    fn resolve_image_path(&self, path: &std::path::Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        }
    }

    /// Refuse — and explain — when the draft already holds the per-message cap.
    ///
    /// Returns whether the cap was hit, so a caller about to do something
    /// expensive (store a pasted image, which is otherwise permanent litter in a
    /// directory nothing sweeps) can stop before doing it. The status is set in
    /// one place so the two entry points cannot explain it differently.
    fn at_image_capacity(&mut self) -> bool {
        if self.attached_images.len() < MAX_IMAGES_PER_MESSAGE {
            return false;
        }
        self.status = self.tr_with(
            TextId::ImageTooMany,
            &[("limit", &MAX_IMAGES_PER_MESSAGE.to_string())],
        );
        true
    }
}

/// A pasted path with the decoration a shell or a terminal adds.
///
/// Terminals paste a dragged file in whatever form the shell would accept:
/// `'/Users/me/My Shot.png'`, `"/Users/me/My Shot.png"`, the backslash-escaped
/// `/Users/me/My\ Shot.png`, or — from an editor or a terminal that models the
/// pasteboard as a URL — `file:///Users/me/My%20Shot.png`. All four name the same
/// file, and refusing any of them would make "paste the path" work only for
/// paths without spaces.
fn unescape_path(raw: &str) -> PathBuf {
    use percent_encoding::percent_decode_str;

    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| {
            trimmed
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or(trimmed);
    let path = match unquoted.strip_prefix("file://") {
        // The host is empty in `file:///Users/…` — the form terminals and editors
        // emit — and `localhost` in the equally valid
        // `file://localhost/Users/…`, where leaving the host in place would turn
        // an absolute path into a relative one.
        Some(rest) => rest.strip_prefix("localhost").unwrap_or(rest),
        None => unquoted,
    };
    let decoded = percent_decode_str(path).decode_utf8_lossy();
    PathBuf::from(decoded.replace("\\ ", " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The images a paste resolves to, or `None` when it is not a paste of images
    /// — the distinction the production code now keeps separate.
    fn pasted_images(app: &App, text: &str) -> Option<Vec<PathBuf>> {
        match app.classify_paste(text) {
            PastedContent::Images(paths) => Some(paths),
            _ => None,
        }
    }

    fn png_path(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1]).unwrap();
        path
    }

    #[test]
    fn a_bare_path_is_recognised() {
        let app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let path = png_path(dir.path(), "shot.png");

        let found = pasted_images(&app, &path.to_string_lossy()).unwrap();

        assert_eq!(found, vec![path]);
    }

    #[test]
    fn quoted_and_escaped_paths_are_recognised() {
        let app = App::new();
        let dir = tempfile::tempdir().unwrap();
        // A directory with a space, so the shell-quoted forms are the realistic
        // ones a terminal actually pastes.
        let spaced = dir.path().join("my shots");
        std::fs::create_dir(&spaced).unwrap();
        let path = png_path(&spaced, "screen shot.png");
        let display = path.to_string_lossy().into_owned();

        for decorated in [
            format!("'{display}'"),
            format!("\"{display}\""),
            display.replace(' ', "\\ "),
        ] {
            let found = pasted_images(&app, &decorated)
                .unwrap_or_else(|| panic!("should recognise {decorated}"));
            assert_eq!(found, vec![path.clone()], "{decorated}");
        }
    }

    #[test]
    fn several_dragged_files_all_attach() {
        let app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let first = png_path(dir.path(), "a.png");
        let second = png_path(dir.path(), "b.png");

        let found = pasted_images(
            &app,
            &format!("{}\n{}\n", first.display(), second.display()),
        )
        .unwrap();

        assert_eq!(found, vec![first, second]);
    }

    /// The rule that keeps this from eating ordinary text: if any line is not
    /// an image path, the paste is text. A log excerpt that mentions a `.png`
    /// has to stay a log excerpt.
    #[test]
    fn a_paste_that_merely_mentions_a_path_stays_text() {
        let app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let path = png_path(dir.path(), "shot.png");

        assert!(pasted_images(&app, &format!("see {} for the layout", path.display())).is_none());
        assert!(
            pasted_images(&app, &format!("{}\nnot a path", path.display())).is_none(),
            "one bad line makes the whole paste text"
        );
        assert!(pasted_images(&app, "plain words").is_none());
        assert!(pasted_images(&app, "").is_none());
    }

    #[test]
    fn a_file_that_is_not_an_image_is_not_a_paste_target() {
        let app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello").unwrap();

        assert!(pasted_images(&app, &path.to_string_lossy()).is_none());
    }

    /// A line that names nothing and a line that names a file we *refuse* are
    /// different answers. Collapsing them into one `None` was why a dragged file
    /// too large to attach landed in the prompt as an unexplained path.
    #[test]
    fn a_refused_file_is_not_the_same_as_a_name_that_is_nowhere() {
        let app = App::new();
        let dir = tempfile::tempdir().unwrap();

        assert!(matches!(
            app.classify_paste("/nowhere/at/all/shot.png"),
            PastedContent::Text
        ));
        assert!(
            matches!(
                app.classify_paste(dir.path().to_str().unwrap()),
                PastedContent::Text
            ),
            "a directory is not a path to an image"
        );

        let oversized = dir.path().join("huge.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(deep_code_agent::MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);
        let refusal = app.classify_paste(&oversized.to_string_lossy());
        assert!(
            matches!(refusal, PastedContent::Unusable { .. }),
            "a file that exists and is too large is a refusal, not text"
        );
    }

    /// A pasted list of paths to files that are not images is *prose* — a log
    /// excerpt, a `find` output — and must stay silent. Only a name that claims
    /// to be an image deserves a line explaining why it did not attach.
    #[test]
    fn a_pasted_list_of_non_image_paths_stays_silent_prose() {
        let mut app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        for name in ["notes.txt", "config.json"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "x").unwrap();
            lines.push(path.to_string_lossy().into_owned());
        }
        let text = lines.join("\n");

        assert!(!app.attach_pasted_images(&text));
        assert!(
            app.status.is_empty(),
            "prose must not produce a status: {}",
            app.status
        );
    }

    /// And the refusal is spoken: the file is named, and the paste still goes in
    /// so nothing the user handed over is swallowed.
    #[test]
    fn a_refused_paste_explains_itself_and_still_inserts_the_text() {
        let mut app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let oversized = dir.path().join("huge.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(deep_code_agent::MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);
        let text = oversized.to_string_lossy().into_owned();

        assert!(!app.attach_pasted_images(&text), "the paste is still text");

        assert!(app.attached_images.is_empty());
        assert!(
            app.status.contains(text.as_str()),
            "the refusal names the file it refused: {}",
            app.status
        );
    }

    /// Two images that each pass on their own can still be a request that cannot
    /// hold them: the per-image cap times the per-message cap is twice the request
    /// budget. At request time the only thing told is the *model* — the user's
    /// chip, the `/image` listing and the status all still say the image is
    /// attached — so the check belongs where the picture is still in their hand.
    /// The boundary the sum check turns on: two images at the per-image cap are
    /// exactly the request budget, so they must both be allowed and only the
    /// third refused. Pinned because `<` instead of `<=` would silently halve
    /// what a user can attach.
    #[test]
    fn the_draft_budget_boundary_is_inclusive() {
        let app = App::new();

        assert!(app.has_room_for(0));
        assert!(app.has_room_for(deep_code_agent::MAX_IMAGE_BYTES));
        assert!(app.has_room_for(deep_code_agent::MAX_TOTAL_BYTES));
        assert!(!app.has_room_for(deep_code_agent::MAX_TOTAL_BYTES + 1));
    }

    #[test]
    fn images_that_together_exceed_the_request_budget_are_refused_while_attaching() {
        let mut app = App::new();
        let dir = tempfile::tempdir().unwrap();
        let mut attached = 0;
        for index in 0..deep_code_agent::MAX_IMAGES_PER_MESSAGE {
            let path = dir.path().join(format!("big-{index}.png"));
            std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
            // Sparse on purpose: `inspect` reads a header and the *metadata*, and
            // the size is what the budget is about, so this costs no 32 MiB.
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(deep_code_agent::MAX_IMAGE_BYTES).unwrap();
            drop(file);
            if !app.attach_image(path) {
                break;
            }
            attached += 1;
        }

        assert_eq!(attached, 2, "two 32 MiB images exhaust a 64 MiB request");
        assert_eq!(app.attached_images.len(), 2);
        assert!(
            app.status.contains("64.0 MiB"),
            "the refusal names the budget: {}",
            app.status
        );
    }

    #[test]
    fn sizes_read_as_the_caps_are_written() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2 KiB");
        assert_eq!(format_size(32 * 1024 * 1024), "32.0 MiB");
    }
}

#[cfg(test)]
mod paste_decoration_tests {
    use super::*;

    fn pasted_images(app: &App, text: &str) -> Option<Vec<PathBuf>> {
        match app.classify_paste(text) {
            PastedContent::Images(paths) => Some(paths),
            _ => None,
        }
    }

    fn app() -> App {
        App::new()
    }

    /// Every spelling a terminal or an editor might hand over for one file.
    #[test]
    fn a_file_url_and_a_percent_escaped_path_name_the_same_image() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("my shots");
        std::fs::create_dir(&spaced).unwrap();
        let path = spaced.join("screen shot.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1]).unwrap();
        let display = path.to_string_lossy().into_owned();

        // `file://` with `%20` for the spaces, which is what a file URL carries.
        let as_url = format!("file://{}", display.replace(' ', "%20"));
        assert_eq!(
            pasted_images(&app, &as_url),
            Some(vec![path.clone()]),
            "{as_url}"
        );
        assert_eq!(
            pasted_images(&app, &format!("'{display}'")),
            Some(vec![path.clone()])
        );
        assert_eq!(
            pasted_images(&app, &display.replace(' ', "\\ ")),
            Some(vec![path])
        );
    }

    /// The confusion this whole path exists for: a terminal's paste key hands
    /// over the *name*, because that is the pasteboard's text representation of
    /// a copied file. The name cannot be resolved — and saying so is the point.
    #[test]
    fn a_bare_image_name_is_reported_rather_than_silently_inserted() {
        assert_eq!(
            App::unresolved_image_name("screen shot.png"),
            Some("screen shot.png".to_string())
        );
        assert_eq!(
            App::unresolved_image_name("/nowhere/at/all/logo.JPEG"),
            Some("logo.JPEG".to_string())
        );
    }

    /// And it must not second-guess ordinary prose. A sentence that happens to
    /// mention a `.png`, or a multi-line paste, is just text.
    #[test]
    fn ordinary_text_is_never_reported_as_an_unresolved_image() {
        assert_eq!(
            App::unresolved_image_name("see shot.png for the layout"),
            None
        );
        assert_eq!(App::unresolved_image_name("shot.png\nother.png"), None);
        assert_eq!(App::unresolved_image_name("logo.svg"), None);
        assert_eq!(App::unresolved_image_name(""), None);
        assert_eq!(App::unresolved_image_name("just words"), None);
    }
}
