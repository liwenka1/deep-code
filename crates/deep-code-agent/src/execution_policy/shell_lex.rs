//! Shell lexical normalization shared by the trust matcher and the deny floor.
//!
//! Both [`super::command_shape`] (the allow side — "is this command trusted?")
//! and [`super::shell_deny`] (the deny side — "is this command catastrophic?")
//! have to agree on ONE thing before they can disagree on anything else: *what
//! will the shell actually execute*. That agreement is these functions —
//! segment splitting, quote/backslash stripping, basename extraction, and the
//! indirection test. They live here, in a module neither side owns, so the
//! allow matcher no longer reaches into the deny module for `clean_token` (which
//! read backwards) and the shared "one view of the shell" invariant has a home.
//!
//! Two readings of a word live here, and which one a rule wants is not a
//! preference. [`clean_token`] is the *text* reading: it deletes quoting
//! wherever it sits, over-approximating on purpose, and it is what the deny
//! floor needs because the floor also covers text a human approved and a shell
//! will re-read. [`executed_words`] is the *argv* reading: the words the
//! executor really passes to `execve`. Any rule whose promise is about what the
//! program receives — the trust fences, the session key — must read that one;
//! the two are equal only by accident, and each accident was a bug.
//!
//! None of this is a full shell parser — it is a deliberate safety
//! over-approximation. Stripping quoting can only ever *expose* a dangerous
//! flag or path, never hide one; an exotic construct falls through to "needs
//! approval" rather than being auto-trusted.

// Inserting a new item between a doc comment and the item it describes
// silently re-parents the prose. That is how the whole "Accepted grammar"
// spec — the authoritative statement of what may run unattended — ended up
// documenting a private `const` while `parse_unattended` had no doc at all;
// nothing flagged it, because the const was private and `missing_docs` only
// looks at public API. This lint does look, and CI runs clippy with
// `-D warnings`.
//
// Its reach, measured on this module rather than assumed — it is a backstop
// for one shape, not a reason to stop reading:
//
//   * Caught: the inserted item has no doc of its own (clippy names the
//     inserter), and the item it stole the prose from is left with none
//     (clippy names the victim). A `pub` item inside these private modules
//     counts as private for this lint, so it is covered too.
//   * Silent: the victim ends up with a doc of its own anyway — that is how
//     the first paragraph of `shell_deny::safety_notes` came to document
//     `SafetyNote`, and nothing flagged it for as long as it stood.
//   * Silent: `#[cfg(test)]` items, which clippy skips. The same re-parenting
//     promptly happened again inside `mod tests`; test items keep their own
//     doc adjacency by declaring helpers inside the function that uses them.
//   * Silent: items this lint treats as exported, and their fields either way
//     — `shell_deny::SafetyNote`'s fields carried no doc with CI green.
//   * Silent: plain `//` comments, which it cannot see at all.
//
// Crate-wide is not the answer either, or not yet: at `lib.rs` this lint
// reports 834 items, which is a documentation project, not a guard.
#![warn(clippy::missing_docs_in_private_items)]

/// The characters [`segments`] cuts a line at: the shell control operators
/// `;`, `&&`, `||`, `|` (each `&` and `|` of a pair is one cut, and the empty
/// piece between them is dropped) and newlines. Named so that the other reader
/// of command boundaries, [`blanks_for_outside_flags`], can be held against
/// it: every character here ends a flag token one way or another
/// ([`Grammar::command_separators`] says which way for each grammar), and
/// `flag_tokens_end_at_every_command_boundary` fails naming any that does not.
pub(super) const SEGMENT_SEPARATORS: [char; 4] = ['\n', ';', '|', '&'];

/// Split a command line into individually-checkable segments on
/// [`SEGMENT_SEPARATORS`]. Each segment is a single simple command whose
/// program/args we can inspect.
///
/// This is a pragmatic tokenizer, not a full shell parser: it does not track
/// quotes or subshells. On the trust side that is a safety bias — a quoted
/// separator only ever cuts a line into *more* segments, and a segment no rule
/// covers costs a prompt, so an exotic construct falls through to "needs
/// approval" rather than being auto-trusted.
///
/// On the deny side the same blindness costs the other direction, and it is
/// the one place this module reads a line no interpreter would run:
/// `git commit -m "cleanup; rm -rf build"` cuts at the quoted `;` and
/// [`super::shell_deny`] refuses the tail as a command, in every tier, on a
/// floor no mode can override. `SECURITY.md` carries it as a written-down
/// residual rather than a rough edge, and
/// `a_quoted_separator_still_cuts_the_line` pins both halves so a
/// quote-aware split has to argue with a test instead of with prose.
pub(super) fn segments(command: &str) -> Vec<&str> {
    command
        .split(SEGMENT_SEPARATORS)
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// `line` with every `\`-newline removed, the way `sh` joins a continued line
/// before it reads a single word of it — or `None` when there is nothing to
/// join (no continuation, or a grammar where `\` is not an escape).
///
/// [`segments`] cuts at every newline, so a continued line came apart into
/// pieces no rule could read: `rm \⏎ -rf /` presented the segment `-rf /`,
/// whose program word is a flag, while the shell ran `rm -rf /`. A line is
/// still read as written as well — this is one MORE reading, never a
/// replacement (see `shell_deny::readings_of`) — because the join is
/// quote-blind like the split: inside `'…'` or a `#` comment a backslash does
/// not continue the line, and reading only the joined text there would fold
/// the next real command into the comment. Read beside the original, a wrong
/// join can only add a finding, never hide one.
///
/// The backslash run is consumed pairwise, so `\\⏎` — an escaped backslash
/// and then a real line break — is left alone, exactly as `sh` leaves it.
pub(super) fn joined_continuations(grammar: Grammar, line: &str) -> Option<String> {
    if !grammar.backslash_escapes || !line.contains("\\\n") {
        return None;
    }
    let mut joined = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            joined.push(c);
            continue;
        }
        match chars.next() {
            Some('\n') => {}
            Some(next) => {
                joined.push('\\');
                joined.push(next);
            }
            None => joined.push('\\'),
        }
    }
    Some(joined)
}

/// Whether [`sh_commands`] reads a here-document's body as more shell lines or
/// steps over it as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeredocBodies {
    /// Step over every body: it is the receiving command's stdin.
    Skip,
    /// Read every body as shell lines, for a line where something may run
    /// what a body feeds it (`sh <<EOF`, `cat <<EOF | sh`, `ssh host <<EOF`).
    Read,
}

/// What [`sh_commands`] read: one argv per simple command, quoting removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ShCommands {
    /// Every simple command on the line, in the order each one ended — a
    /// command substitution's commands come before the command it sits in.
    pub(super) commands: Vec<Vec<String>>,
    /// Whether a here-document body was stepped over, so the caller knows a
    /// [`HeredocBodies::Read`] pass could read more.
    pub(super) skipped_heredoc: bool,
    /// The words given as here-strings (`bash <<< 'npm publish'`): stdin text
    /// like a body, though each stays an operand in `commands` too.
    pub(super) here_strings: Vec<String>,
    /// How many of `commands`, from the front, had finished before the first
    /// here-document or here-string fed anything — `cargo build` in `cargo
    /// build && git commit -F - <<EOF`. None of those can run what was fed.
    /// Any command from here on can: the pipeline it is fed to, whatever that
    /// pipeline's output becomes (`bash -c "$(cat <<EOF …)"`), and every later
    /// command, which can run the file or variable it was stored in (`cat >
    /// x.sh <<EOF … EOF⏎./x.sh`, `s=$(cat <<EOF …); sh -c "$s"`).
    ///
    /// "Finished" is read narrowly: an earlier pipeline at the line's top
    /// level, ended by `;`, `&&`, `||` or a newline. Not a stage piped into
    /// the fed pipeline, which runs alongside it (`|⏎` continues a pipeline,
    /// and a `{ … }` or `if` that is piped into is one stage with what feeds
    /// it); not anything inside a loop that the fed command sits in, which the
    /// next iteration runs again; and nothing at all once a background `&`
    /// came first, since what it started may still be running. 0 when nothing
    /// was fed.
    pub(super) finished_before_heredoc: usize,
}

/// The simple commands of `command` as `sh` splits them, quote-aware: words on
/// unquoted blanks, `'…'` and `"…"` removed, `\` escapes, a `\`-newline joined,
/// `#` comments dropped, commands cut at unquoted `;` `&` `|` and newlines, and
/// `( … )`, `$( … )` and `` ` … ` `` read as commands of their own — inside
/// double quotes too, where the shell still runs them. A here-document body is
/// stepped over or read per `bodies`.
///
/// `None` for anything it does not model, so the caller falls back to the
/// quote-blind [`segments`]: an unterminated quote or substitution, a trailing
/// backslash, `$'…'`/`$"…"`, an escaped backtick inside backticks, and every
/// grammar but `sh`'s (`cmd` has no `'`, escapes with `^`, and reads `%`).
///
/// Not a gate and not the executor's reading ([`parse_unattended`] is both). It
/// exists for the irreversible-outward floor, whose job is to ask before a
/// *routine* irreversible spelling runs, and which the quote-blind split made
/// read data as commands: `git commit -m "…⏎⏎npm publish now runs in CI"` cut at
/// the quoted newline into the segment `npm publish now runs in CI`, and a
/// heredoc'd commit message the same way — where nobody can answer the
/// question, that is a refused commit.
///
/// One pass, linear: every character is looked at once, a quoted run is
/// scanned to its close and jumped, and a skipped body is scanned line by line
/// to its delimiter.
pub(super) fn sh_commands(command: &str, bodies: HeredocBodies) -> Option<ShCommands> {
    sh_commands_in(HOST, command, bodies)
}

/// [`sh_commands`] under an explicit [`Grammar`], so its reading is pinned from
/// every host and its refusal of the `cmd` grammar is too.
pub(super) fn sh_commands_in(
    grammar: Grammar,
    command: &str,
    bodies: HeredocBodies,
) -> Option<ShCommands> {
    if grammar != SH {
        return None;
    }
    let chars: Vec<char> = command.chars().collect();
    let mut lexer = ShLexer {
        frames: vec![LexFrame::Commands(CommandFrame::new(None, false, false))],
        read: ShCommands::default(),
        pending: Vec::new(),
        after_redirect: false,
        after_pipe: false,
        here_string_next: false,
        pipeline_start: 0,
        compounds: Vec::new(),
        backgrounded: false,
        fence: None,
        held: Vec::new(),
    };
    let mut i = 0;
    while i < chars.len() {
        i = lexer.step(&chars, i, bodies)?;
    }
    if lexer.frames.len() != 1 {
        // An unclosed `"`, `$(`, `(` or backtick: `sh` refuses the line.
        return None;
    }
    lexer.end_pipeline();
    lexer.read.finished_before_heredoc = lexer.fence.unwrap_or(0);
    Some(lexer.read)
}

/// Reserved words after which the next word is still in command position
/// (`if ! { cmd`), so a reserved word there is one too.
const LEADS_A_COMMAND: &[&str] = &[
    "!", "{", "if", "then", "else", "elif", "while", "until", "do", "time",
];

/// One open context of [`sh_commands`].
enum LexFrame {
    /// A run of commands: the line itself, `( … )`/`$( … )` (closed by `)`), or
    /// `` ` … ` `` (closed by a backtick).
    Commands(CommandFrame),
    /// Inside `"…"`. Its text belongs to the word of the `Commands` frame below.
    DoubleQuoted,
}

/// The state of one [`LexFrame::Commands`].
struct CommandFrame {
    /// What closes this frame: `None` for the line itself.
    closer: Option<char>,
    /// Whether `<<` is a shift here rather than a here-document: `(( … ))`,
    /// `$(( … ))` and a group inside either.
    arithmetic: bool,
    /// Whether the words here are data rather than commands: an array
    /// (`name=( … )`, which dash refuses) and a group inside one. NOT `(( … ))`
    /// at the head of a command: bash reads that as arithmetic, but dash —
    /// `/bin/sh` on Debian and Ubuntu — reads it as two nested subshells and
    /// RUNS what is inside, so its words are read as a command. A substitution
    /// inside a data frame still opens a frame of commands of its own.
    discard: bool,
    /// Whether the commands read here are held back until the `$((` they sit
    /// in is decided (see `undecided` and `ShLexer::held`): its own frame and
    /// every group inside it. A substitution inside records its own, which
    /// run either way.
    holds: bool,
    /// Whether this is the inner frame of a `$((`. Every `sh` reads `$(( …
    /// ))` as arithmetic, but bash reads a `$((` whose `((` does not close as
    /// `))` as a substitution holding a subshell, and runs it: `$((cd x &&
    /// npm publish) | sh)`. Only the frame's `)` tells them apart.
    undecided: bool,
    /// `case` statements open in this frame, so a pattern's `)` is not taken
    /// for the frame's closer (`"$(case $x in a) npm publish;; esac)"`).
    open_cases: u32,
    /// Whether the next word is in command position: only reserved words
    /// that lead a command (`if`, `!`, `then`, …) came before it in this one.
    command_position: bool,
    /// The words of the command being read.
    argv: Vec<String>,
    /// The word being read, once anything (even `""`) has opened it.
    word: Option<String>,
    /// Whether any of that word was quoted or escaped, so it is no reserved
    /// word (`"done"` is a command name, `done` closes a loop).
    quoted: bool,
}

impl CommandFrame {
    /// An empty frame that `closer` ends (`None`: the line itself).
    fn new(closer: Option<char>, arithmetic: bool, discard: bool) -> Self {
        Self {
            closer,
            arithmetic,
            discard,
            holds: false,
            undecided: false,
            open_cases: 0,
            command_position: true,
            argv: Vec::new(),
            word: None,
            quoted: false,
        }
    }
}

/// A compound command (`{ … }`, `if`, `case`, a loop) open at the line's top
/// level.
struct OpenCompound {
    /// Where the pipeline holding it began. It is one stage of that pipeline,
    /// so what follows its closer (`done <<EOF`, `} | sh`) belongs to it too.
    pipeline_start: usize,
    /// The most `ShCommands::finished_before_heredoc` can be for a body fed
    /// inside it, if it or a compound around it is a loop or is piped into.
    fence_cap: Option<usize>,
}

/// A here-document waiting for the newline after which its body starts.
struct PendingHeredoc {
    /// The delimiter word, quotes removed.
    delimiter: Vec<char>,
    /// `<<-`: leading tabs are stripped before the delimiter is compared.
    strip_tabs: bool,
}

/// The state of one [`sh_commands`] pass.
struct ShLexer {
    /// Open contexts, innermost last; the bottom one is the line itself.
    frames: Vec<LexFrame>,
    /// What has been read so far.
    read: ShCommands,
    /// Here-documents opened on the current line, in order.
    pending: Vec<PendingHeredoc>,
    /// Whether the last character added to the current word was an UNQUOTED
    /// `<` or `>`, so a `&` right after it is a redirection's (`2>&1`) and not
    /// a separator — a quoted one (`echo "<"& npm publish`) is just text.
    after_redirect: bool,
    /// Whether nothing but blanks and comments followed the last `|`, so a
    /// newline continues its pipeline (`cat <<EOF |⏎…⏎EOF⏎sh` runs the body).
    after_pipe: bool,
    /// Whether the next word ends a here-string (`<<< WORD`).
    here_string_next: bool,
    /// Where the line's current top-level pipeline began: how many commands
    /// had ended before it.
    pipeline_start: usize,
    /// Compound commands open at the line's top level, innermost last.
    compounds: Vec<OpenCompound>,
    /// Whether a `&` has put something in the background.
    backgrounded: bool,
    /// `ShCommands::finished_before_heredoc`, fixed at the first `<<`/`<<<`.
    fence: Option<usize>,
    /// The commands held for each `$((` still undecided, innermost last. A
    /// frame that holds pushes to the innermost, which is its own: any `$((`
    /// opened later inside it has been decided by the time it reads again.
    /// Each held command is moved once, when its `$((` is decided.
    held: Vec<Vec<Vec<String>>>,
}

impl ShLexer {
    /// The innermost `Commands` frame. A `DoubleQuoted` frame only ever sits
    /// directly on a `Commands` one, so this looks at most one frame down.
    fn command_frame(&mut self) -> &mut CommandFrame {
        for frame in self.frames.iter_mut().rev() {
            if let LexFrame::Commands(frame) = frame {
                return frame;
            }
        }
        unreachable!("the bottom frame is always a Commands frame")
    }

    /// Append `ch` to the word being read.
    fn push(&mut self, ch: char) {
        self.after_redirect = false;
        self.after_pipe = false;
        self.command_frame()
            .word
            .get_or_insert_with(String::new)
            .push(ch);
    }

    /// Make sure a word exists even if nothing is pushed into it (`""`, or a
    /// substitution standing for a whole word).
    fn touch_word(&mut self) {
        self.after_pipe = false;
        self.command_frame().word.get_or_insert_with(String::new);
    }

    /// Note that the word being read has a quoted or escaped part.
    fn quote_word(&mut self) {
        self.touch_word();
        self.command_frame().quoted = true;
    }

    /// Close the word being read, if one is open. An unquoted reserved word in
    /// command position opens or closes a `case` in this frame and, at the
    /// line's top level, a compound command.
    fn end_word(&mut self) {
        self.after_redirect = false;
        let frame = self.command_frame();
        let Some(word) = frame.word.take() else {
            return;
        };
        let reserved = !std::mem::take(&mut frame.quoted) && frame.command_position;
        frame.command_position = reserved && LEADS_A_COMMAND.contains(&word.as_str());
        if reserved {
            match word.as_str() {
                "case" => frame.open_cases += 1,
                "esac" => frame.open_cases = frame.open_cases.saturating_sub(1),
                _ => {}
            }
        }
        let top_level = frame.closer.is_none();
        if std::mem::take(&mut self.here_string_next) {
            self.read.here_strings.push(word.clone());
        }
        if reserved && top_level {
            self.compound_word(&word);
        }
        self.command_frame().argv.push(word);
    }

    /// A reserved word in command position at the line's top level: open or
    /// close a compound command (see `ShCommands::finished_before_heredoc`).
    fn compound_word(&mut self, word: &str) {
        match word {
            "{" | "if" | "case" | "while" | "until" | "for" | "select" => {
                let is_loop = matches!(word, "while" | "until" | "for" | "select");
                let piped_into = self.read.commands.len() > self.pipeline_start;
                let own = (is_loop || piped_into).then_some(self.pipeline_start);
                let around = self.compounds.last().and_then(|outer| outer.fence_cap);
                self.compounds.push(OpenCompound {
                    pipeline_start: self.pipeline_start,
                    fence_cap: own.into_iter().chain(around).min(),
                });
            }
            "}" | "fi" | "esac" | "done" => {
                if let Some(compound) = self.compounds.pop() {
                    self.pipeline_start = compound.pipeline_start;
                }
            }
            _ => {}
        }
    }

    /// Close the command being read in the innermost frame and record it, or
    /// hold it there until the `$((` around it is decided.
    fn end_command(&mut self) {
        self.end_word();
        let frame = self.command_frame();
        let argv = std::mem::take(&mut frame.argv);
        frame.command_position = true;
        if argv.is_empty() || frame.discard {
            return;
        }
        let holds = frame.holds;
        match self.held.last_mut() {
            Some(held) if holds => held.push(argv),
            _ => self.read.commands.push(argv),
        }
    }

    /// Close the command being read AND the pipeline it ends (any separator
    /// but a single `|`). At the line's top level the next pipeline starts
    /// after every command read so far.
    fn end_pipeline(&mut self) {
        self.end_command();
        if self.command_frame().closer.is_none() {
            self.pipeline_start = self.read.commands.len();
        }
    }

    /// A here-document or here-string feeds the command being read. The first
    /// one fixes how many commands had finished before anything was fed (see
    /// `ShCommands::finished_before_heredoc`).
    fn mark_fed(&mut self) {
        if self.fence.is_none() {
            let cap = self.compounds.last().and_then(|open| open.fence_cap);
            let fence = if self.backgrounded {
                0
            } else {
                cap.map_or(self.pipeline_start, |cap| cap.min(self.pipeline_start))
            };
            self.fence = Some(fence);
        }
    }

    /// Open a frame that `closer` ends.
    fn open(&mut self, closer: char, arithmetic: bool, discard: bool) {
        self.after_pipe = false;
        self.frames.push(LexFrame::Commands(CommandFrame::new(
            Some(closer),
            arithmetic,
            discard,
        )));
    }

    /// Open `( … )`, or the inner frame of `(( … ))`: a group of the frame it
    /// sits in, so the words of an array, of arithmetic or of a `$((` yet to
    /// be decided stay what they are there.
    fn open_group(&mut self, arithmetic: bool) {
        let around = self.command_frame();
        let arithmetic = arithmetic || around.arithmetic;
        let discard = around.discard;
        let holds = around.holds;
        self.open(')', arithmetic, discard);
        self.command_frame().holds = holds;
    }

    /// Open the inner frame of a `$((`, which its `)` decides (see
    /// `CommandFrame::undecided`).
    fn open_undecided(&mut self) {
        self.open(')', true, false);
        self.held.push(Vec::new());
        let frame = self.command_frame();
        frame.holds = true;
        frame.undecided = true;
    }

    /// End the innermost frame of commands at its closer, `first_of_double`
    /// when that closer is the first `)` of `))`. A `$((` frame drops what was
    /// held for it if it closed as `))` — it was arithmetic — and records it
    /// if not.
    fn close(&mut self, first_of_double: bool) {
        self.end_pipeline();
        let Some(LexFrame::Commands(frame)) = self.frames.pop() else {
            unreachable!("only a Commands frame has a closer")
        };
        if frame.undecided {
            let held = self.held.pop().unwrap_or_default();
            if !first_of_double {
                self.read.commands.extend(held);
            }
        }
    }

    /// Read from `chars[i]`; the index to continue from, or `None` for a line
    /// this lexer does not model.
    fn step(&mut self, chars: &[char], i: usize, bodies: HeredocBodies) -> Option<usize> {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let (closer, arithmetic) = match self.frames.last() {
            Some(LexFrame::DoubleQuoted) => return Some(self.step_quoted(chars, i)),
            Some(LexFrame::Commands(frame)) => (frame.closer, frame.arithmetic),
            None => unreachable!("the bottom frame is never popped"),
        };
        match c {
            '\\' => match next {
                None => None,
                Some('\n') => Some(i + 2),
                // A nested backtick spelled `\``: not modelled.
                Some('`') if closer == Some('`') => None,
                Some(escaped) => {
                    self.quote_word();
                    self.push(escaped);
                    Some(i + 2)
                }
            },
            '\'' => {
                let close = chars[i + 1..].iter().position(|&ch| ch == '\'')? + i + 1;
                self.quote_word();
                for &ch in &chars[i + 1..close] {
                    self.push(ch);
                }
                Some(close + 1)
            }
            '"' => {
                self.quote_word();
                self.after_redirect = false;
                self.frames.push(LexFrame::DoubleQuoted);
                Some(i + 1)
            }
            '$' => match next {
                // `$((`: arithmetic or a substitution holding a subshell,
                // which only its `)` decides (see `CommandFrame::undecided`).
                Some('(') if chars.get(i + 2) == Some(&'(') => {
                    self.touch_word();
                    self.open(')', false, false);
                    self.open_undecided();
                    Some(i + 3)
                }
                Some('(') => {
                    self.touch_word();
                    self.open(')', false, false);
                    Some(i + 2)
                }
                Some('\'' | '"') => None,
                _ => {
                    self.push('$');
                    Some(i + 1)
                }
            },
            '`' if closer == Some('`') => {
                self.close(false);
                Some(i + 1)
            }
            '`' => {
                self.touch_word();
                self.open('`', false, false);
                Some(i + 1)
            }
            // The second `(` of `((`: arithmetic to bash, a subshell to dash
            // (see `CommandFrame::discard`), so its words are still a command.
            '(' if i > 0 && chars[i - 1] == '(' => {
                self.open_group(true);
                Some(i + 1)
            }
            // `name=(…)`: an array's elements, data.
            '(' if self
                .command_frame()
                .word
                .as_deref()
                .is_some_and(|word| word.ends_with('=')) =>
            {
                self.open(')', false, true);
                Some(i + 1)
            }
            '(' => {
                self.end_command();
                self.open_group(false);
                Some(i + 1)
            }
            ')' => {
                // The word before it first: `esac)` closes the `case`, then
                // this `)` closes the frame.
                self.end_word();
                if closer == Some(')') && self.command_frame().open_cases == 0 {
                    self.close(next == Some(')'));
                } else {
                    // A `case` pattern's `)`: it ends what came before it.
                    self.end_pipeline();
                }
                Some(i + 1)
            }
            '#' if self.command_frame().word.is_none() => {
                // A comment runs to the newline, which is left for the next
                // step to end the command on.
                let end = chars[i..]
                    .iter()
                    .position(|&ch| ch == '\n')
                    .map_or(chars.len(), |offset| i + offset);
                Some(end)
            }
            ' ' | '\t' => {
                self.end_word();
                Some(i + 1)
            }
            '\n' => {
                // A newline right after a `|` continues its pipeline.
                if !std::mem::take(&mut self.after_pipe) {
                    self.end_pipeline();
                }
                Some(self.after_newline(chars, i + 1, bodies))
            }
            '&' if self.after_redirect => {
                // `2>&1`, `>&2`: a redirection's `&`, not a separator.
                self.push('&');
                Some(i + 1)
            }
            '&' if next == Some('&') => {
                self.end_pipeline();
                Some(i + 2)
            }
            // `>|` overrides noclobber: a redirection, not a pipe.
            '|' if self.after_redirect => {
                self.push('|');
                Some(i + 1)
            }
            // A pipe hands this command's output to the next one: same
            // pipeline. `|&` pipes stderr too.
            '|' if next != Some('|') => {
                self.end_command();
                self.after_pipe = true;
                Some(if next == Some('&') { i + 2 } else { i + 1 })
            }
            '|' => {
                self.end_pipeline();
                Some(i + 2)
            }
            ';' | '&' => {
                // A lone `&` runs what it ends in the background, on beside
                // whatever follows (bash's `&>` is a redirection instead).
                if c == '&' && next != Some('>') {
                    self.backgrounded = true;
                }
                self.end_pipeline();
                Some(i + 1)
            }
            '<' if next == Some('<') && chars.get(i + 2) == Some(&'<') => {
                // A here-string: its word is an operand, and stdin text like a
                // here-document's body.
                self.end_word();
                self.mark_fed();
                self.here_string_next = true;
                Some(i + 3)
            }
            // In arithmetic `<<` is a shift; read it as text.
            '<' if next == Some('<') && !arithmetic => self.heredoc(chars, i + 2),
            '<' | '>' => {
                self.push(c);
                self.after_redirect = true;
                Some(i + 1)
            }
            _ => {
                self.push(c);
                Some(i + 1)
            }
        }
    }

    /// One character inside `"…"`: only `"`, `\`, `$(` and a backtick are
    /// special there.
    fn step_quoted(&mut self, chars: &[char], i: usize) -> usize {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '"' => {
                self.frames.pop();
                i + 1
            }
            '\\' => match next {
                // An unterminated escape at the end: the unclosed quote makes
                // the whole line `None` anyway.
                None => i + 1,
                Some('\n') => i + 2,
                Some(escaped @ ('"' | '\\' | '$' | '`')) => {
                    self.push(escaped);
                    i + 2
                }
                Some(other) => {
                    self.push('\\');
                    self.push(other);
                    i + 2
                }
            },
            '$' if next == Some('(') && chars.get(i + 2) == Some(&'(') => {
                self.open(')', false, false);
                self.open_undecided();
                i + 3
            }
            '$' if next == Some('(') => {
                self.open(')', false, false);
                i + 2
            }
            '`' => {
                self.open('`', false, false);
                i + 1
            }
            _ => {
                self.push(c);
                i + 1
            }
        }
    }

    /// `<<` or `<<-` at `chars[start - 2]`: read the delimiter word (quotes
    /// removed) and queue the body for the next newline.
    fn heredoc(&mut self, chars: &[char], start: usize) -> Option<usize> {
        self.end_word();
        self.mark_fed();
        let mut i = start;
        let strip_tabs = chars.get(i) == Some(&'-');
        if strip_tabs {
            i += 1;
        }
        while matches!(chars.get(i), Some(' ' | '\t')) {
            i += 1;
        }
        let mut delimiter = Vec::new();
        while let Some(&ch) = chars.get(i) {
            match ch {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
                '\'' | '"' => {
                    let close = chars[i + 1..].iter().position(|&c| c == ch)? + i + 1;
                    delimiter.extend_from_slice(&chars[i + 1..close]);
                    i = close + 1;
                }
                '\\' => {
                    delimiter.push(*chars.get(i + 1)?);
                    i += 2;
                }
                _ => {
                    delimiter.push(ch);
                    i += 1;
                }
            }
        }
        if delimiter.is_empty() {
            return None;
        }
        self.pending.push(PendingHeredoc {
            delimiter,
            strip_tabs,
        });
        Some(i)
    }

    /// Where reading resumes after a newline at `start - 1`: past the bodies of
    /// the here-documents that newline opened, or right there.
    fn after_newline(&mut self, chars: &[char], start: usize, bodies: HeredocBodies) -> usize {
        if self.pending.is_empty() {
            return start;
        }
        if bodies == HeredocBodies::Read {
            // The bodies are read in place, as the lines they are.
            self.pending.clear();
            return start;
        }
        self.read.skipped_heredoc = true;
        let mut i = start;
        for doc in std::mem::take(&mut self.pending) {
            loop {
                if i >= chars.len() {
                    // `sh` ends an unterminated body at the end of input.
                    return chars.len();
                }
                let end = chars[i..]
                    .iter()
                    .position(|&ch| ch == '\n')
                    .map_or(chars.len(), |offset| i + offset);
                let mut line = &chars[i..end];
                if doc.strip_tabs {
                    while let [first, rest @ ..] = line {
                        if *first != '\t' {
                            break;
                        }
                        line = rest;
                    }
                }
                i = end + 1;
                if line == doc.delimiter.as_slice() {
                    break;
                }
            }
        }
        i.min(chars.len())
    }
}

/// Words the shell consumes ahead of a segment's program word: control-flow
/// reserved words (`if true; then rm -rf /; fi` lexes to a segment starting
/// with `then`), the `!` negation, `time`, and the transparent wrappers whose
/// whole job is to run the rest of the line unchanged (`exec rm -rf /`,
/// `env X=1 rm -rf /`, `echo / | xargs rm -rf`).
///
/// The two sides read this list in opposite directions, which is why it lives
/// here and not in either of them. The deny floor reads *past* these words to
/// find the program they hand off to. The identity side treats a line that
/// *opens* with one as naming no operation of its own — `time <anything>` —
/// so it never collapses to the bare word (see
/// [`super::command_shape::identity`]). Wrappers that take their own options
/// first (`nice -n 5 …`, `timeout 5 …`, `env -i …`) are not parsed on the deny
/// side: their option becomes the "program" and matches no rule — the
/// wrapper-options arms race the floor deliberately stays out of (such a line
/// is never trusted and never a bounded edit, so it still lands on a human).
/// Listing such a wrapper is still worth it for the identity side, which only
/// needs to know the line names no operation of its own.
///
/// Membership is not a memory exercise: `every_word_the_shell_runs_the_tail_for_is_known`
/// asks the real shell which candidates hand off execution and fails naming
/// the ones missing from here. That test is what found `caffeinate` and
/// `xcrun`; the entries below them are their Linux counterparts, added
/// pre-emptively because this list is one-directional — an entry the local
/// shell does not treat as a wrapper costs nothing but a literal identity.
pub(super) const PREFIX_WORDS: &[&str] = &[
    "if",
    "then",
    "else",
    "elif",
    "while",
    "until",
    "do",
    "for",
    "case",
    "!",
    "time",
    "exec",
    "command",
    "builtin",
    "env",
    "nohup",
    "nice",
    "busybox",
    "xargs",
    "caffeinate",
    "xcrun",
    "setsid",
    "stdbuf",
    "timeout",
    "ionice",
    "taskset",
    "unshare",
    "chroot",
    "flock",
    "runuser",
    "arch",
    "watch",
    "script",
    "proxychains",
    "parallel",
];

/// Interpreters that execute whatever text they are handed — a script path, a
/// `-c` string, or piped stdin. The deny floor uses the list to refuse a
/// network fetch piped into one; the identity side uses it the way it uses
/// [`PREFIX_WORDS`]: `sh -c '<anything>'` and `python <any script>` name no
/// operation of their own, so a consent on one must not cover another.
///
/// Membership rule, so the next addition is not a judgement call: a runtime
/// that executes text supplied at the call site. That is why the shell's own
/// `eval`, `source` and `.` belong here — they were missing, so one session
/// "a" on `source .venv/bin/activate` covered `source ./anything.sh` for the
/// rest of the session, and in AcceptEdits the model writes that script with
/// no prompt either. `awk`/`osascript` are here for the same reason a level
/// down: their program text can `system()` / `do shell script` out.
pub(super) const INTERPRETERS: &[&str] = &[
    // The shell's own text-executing builtins.
    "eval",
    "source",
    ".",
    "awk",
    "gawk",
    "osascript",
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "perl",
    "python",
    "python3",
    "ruby",
    "node",
    "php",
    // Windows interpreters were missing, so `curl x | powershell` — the
    // standard Windows one-line installer shape — was not denied on ANY
    // platform.
    "powershell",
    "pwsh",
    "cmd",
];

/// Whether a program word (lowercased basename) runs whatever the rest of the
/// line names instead of an operation of its own: a [`PREFIX_WORDS`] wrapper
/// or an [`INTERPRETERS`] entry.
pub(super) fn runs_the_rest_of_the_line(program: &str) -> bool {
    PREFIX_WORDS.contains(&program) || INTERPRETERS.contains(&program)
}

/// What this module has to know about the argument grammar of the platform an
/// unattended command is read by: whether `\` escapes, what separates path
/// components, and what the *text* reading deletes.
///
/// One value, not a `cfg!(windows)` at each site. Each platform question so far
/// was answered locally where it came up, and the answers drifted apart: the
/// escape question reached the top-level backslash arm and the double-quote
/// loop but not the single-quote loop, so the one spelling the parser claims to
/// refuse was read two different ways depending on which quote consumed it.
/// A value is also reachable by name, so both readings are pinned by a table
/// that runs on macOS, Linux and Windows alike instead of by `#[cfg]` blocks
/// that only ever execute on the platform they describe — every round of "green
/// here, red on Windows CI" was a `cfg` block nobody could run.
///
/// Only the parser is parameterized by this value today. [`clean_token`] and
/// [`basename_lower`] read [`HOST`] (their `_in` twins exist for tests), so a
/// Windows expectation must not be built by mixing them: parsing with
/// [`WINDOWS`] and then de-quoting with the host's reading is a hybrid no
/// platform has. What is reachable by name is the parse and the two lexical
/// readings; `shell_deny`'s end-to-end verdicts are still pinned per platform
/// by `#[cfg]`, so the Windows reading of `r\m -rf /` is asserted on Windows
/// only.
///
/// Two things are deliberately *not* in here. Which characters quote a word:
/// both grammars quote with `'` and `"`, which is this parser's own choice
/// rather than a platform fact (`cmd.exe` has no `'`), and it can be, because
/// the words this parser produces *are* the argv the executor hands to the OS —
/// its reading is the one that runs, and one reading for both platforms is what
/// lets the fences above it be written once. And
/// [`escapes_cwd_by_spelling`], which stays platform-blind one level up for a
/// different reason: that fence may only ever over-refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Grammar {
    /// Whether `\` escapes the character after it (and a `\`-newline continues
    /// the line).
    ///
    /// It does in `sh`. It does not on Windows, where it is the path separator
    /// and `CreateProcess` — which is what an unattended command reaches, with
    /// no `cmd.exe` in between — never had a backslash escape. Reading it as an
    /// escape there silently ate every ordinary path spelling: `git diff
    /// src\main.rs` ran as `git diff srcmain.rs` on the default-trusted `git
    /// diff`, and `.\.` collapsed to `..`, which is a *different directory*
    /// than the operand fence read.
    pub(super) backslash_escapes: bool,
    /// What separates path components for [`basename_lower`], so `/usr/bin/sudo`,
    /// `'sudo'` and `C:\Windows\System32\reg.exe` all resolve to the program
    /// word a rule names.
    pub(super) path_separators: &'static [char],
    /// What the *text* reading ([`clean_token`]) deletes: the quote characters,
    /// plus the platform's escape character.
    ///
    /// That is `\` on Unix (a backslash escapes the next char, so `\-rf` runs
    /// as `-rf` and `\/tmp` as `/tmp`) and `^` on Windows — cmd.exe's escape
    /// character, the exact counterpart. Without stripping the caret, one of
    /// them walked past every rule on the deny floor (`r^d /s /q C:\Windows`,
    /// `de^l /f/s/q C:\*`, `curl x | powershe^ll`) while `cmd /C` ran the real
    /// thing, and Windows is the one platform with no sandbox behind that
    /// floor. A Windows `\` is not stripped: there it is a genuine path
    /// separator, not quoting.
    pub(super) quoting_to_strip: &'static [char],
    /// Characters with which the platform's command interpreter rewrites a
    /// *word* before the program word exists, beyond the ones both platforms
    /// share (`$`, a backtick, braces, globs — all in
    /// [`has_shell_indirection`] unconditionally).
    ///
    /// Empty for `sh`, whose rewriting punctuation is already on that shared
    /// list. `%` for Windows: `cmd.exe` expands `%VAR%` and its substring and
    /// replace forms (`%VAR:~0,0%`, `%VAR:a=b%`) on the command line, so
    /// `de%PATH:~0,0%l /f/s/q C:\*` is `del /f/s/q C:\*` by the time anything
    /// runs. It cannot be on the shared list: on Unix two `%` in one word is
    /// ordinary (`date +%Y%m%d`, `git log --format=%h%s`), so refusing them
    /// there would block real work for nothing.
    pub(super) rewrites_words_with: &'static [char],
    /// What the platform's interpreter treats as a word delimiter *besides*
    /// blanks, and which it is therefore safe to read as one everywhere on the
    /// line.
    ///
    /// Empty for `sh`, which delimits on blanks alone. `,` and `;` for
    /// `cmd.exe`: `del,/f/s/q,C:\*` and `del;/f/s/q;C:\*` are both `del` with
    /// three arguments there, and one opaque word to anything that splits on
    /// blanks — which is how they walked past every rule on the deny floor.
    ///
    /// `;` was held out of this set for one release on the grounds that
    /// normalizing it merges two commands, which made
    /// `curl https://x -o f; echo hi | sh` read as a fetch feeding the pipe and
    /// denied it in every mode. The measurement was real; the conclusion was
    /// wrong. This set is non-empty only under the Windows grammar, and *there*
    /// the merge is what `cmd` does — `;` delimits words for it while `|` still
    /// pipes, so that line really is a fetch feeding `sh`, and denying it is the
    /// right answer. The reading a human means by `;` is the Unix one, where
    /// this set is empty and nothing is merged.
    ///
    /// Merging can only add words to a segment, never take its program word
    /// away, so no per-segment rule can lose a denial to it. What it could
    /// invent is a rule that ORs over arguments seeing an argument that belonged
    /// to the next command (`rm -r ./build; echo ~`). Every such rule needs a
    /// shape no ordinary second command has — `rm -rf` is denied by its flags
    /// alone, so the iconic spelling is untouched — and the cost is confined to
    /// the one platform whose interpreter really reads the line that way.
    pub(super) word_delimiters: &'static [char],
    /// Word delimiters that may only be read as such *outside a flag token* —
    /// a token starting with `-`.
    ///
    /// `=` for `cmd.exe`, and that restriction is the whole reason it is a
    /// field of its own. Read as a delimiter everywhere, it invented denials no
    /// mode can override, measured on ordinary lines: `rm -r --exclude=/ build`
    /// became a recursive remove of `/`. Read only outside flags, it still
    /// recovers the spelling that walked past the floor — `del=/f/s/q C:\*`,
    /// where the `=` makes the verb look like an environment assignment to
    /// [`super::shell_deny`] and hands the program word to `C:\*`.
    ///
    /// `format=ntfs C:` is denied under this reading, and that is correct rather
    /// than the false positive it was once recorded as: to `cmd`, that line *is*
    /// the `format` program with a drive operand. Under the `sh` grammar it
    /// stays an environment assignment, this set is empty, and nothing about it
    /// changes.
    pub(super) word_delimiters_outside_flags: &'static [char],
    /// Where one command ends and the next begins for this interpreter, which
    /// is where a flag token ends too ([`blanks_for_outside_flags`]).
    ///
    /// A flag glued to one of these used to carry the `=` carve-out into the
    /// next command: `echo --x=1|del=/f/s/q C:\*` is two commands to `cmd` and
    /// the second is the drive wipe, and with the `|` inside the token the `=`
    /// hiding the verb was never read.
    ///
    /// `;` is the one character the two grammars disagree about, which is why
    /// this is a field and not a constant. It ends a command in `sh`. To `cmd`
    /// it is a *word* delimiter — it is in [`Grammar::word_delimiters`] there —
    /// so it must stay inside the token: that is what reads the `=` in
    /// `del;-x=/s;C:\*`, whose token begins with `d`, and the closure in
    /// `shell_deny::readings_of` takes the result on to `del -x /s C:\*`, which
    /// is what `cmd` runs. Cutting there would have made `-x=/s` a flag and
    /// handed the spelling to the residual `SECURITY.md` records for a
    /// delimiter inside a flag's value. Written as one module-wide constant,
    /// `cmd`'s answer was simply asserted for both, and `sh` was right only
    /// because its `=` set is empty and the function returns before ever asking.
    ///
    /// A newline is here for completeness and costs nothing: it is whitespace,
    /// which ends a token anyway.
    pub(super) command_separators: &'static [char],
    /// Whether a word opening with `#` comments out the rest of the line.
    ///
    /// It does in `sh`. `cmd.exe` has no comment syntax at all, and `#` is a
    /// legal filename character there, so reading it as one silently deleted
    /// operands from the argv the executor runs: `git log --grep #123` parsed
    /// to `["git", "log", "--grep"]` — trusted, because `git log` is — and
    /// `type #notes.md` to `["type"]`.
    pub(super) comments_with_hash: bool,
}

/// The grammar of `sh`, which is how an unattended command's words are read on
/// Unix: the executor runs the argv directly, but every fence above it was
/// written against `sh`, and `unattended_parse_matches_sh_word_splitting` holds
/// the two together against the real shell.
pub(super) const SH: Grammar = Grammar {
    backslash_escapes: true,
    path_separators: &['/'],
    quoting_to_strip: &['\'', '"', '\\'],
    rewrites_words_with: &[],
    word_delimiters: &[],
    word_delimiters_outside_flags: &[],
    command_separators: &['\n', ';', '|', '&'],
    comments_with_hash: true,
};

/// The Windows grammar: `\` is a path separator rather than an escape, both
/// separators are real, and the escape character to strip is cmd.exe's `^`.
pub(super) const WINDOWS: Grammar = Grammar {
    backslash_escapes: false,
    path_separators: &['/', '\\'],
    quoting_to_strip: &['\'', '"', '^'],
    rewrites_words_with: &['%'],
    word_delimiters: &[',', ';'],
    word_delimiters_outside_flags: &['='],
    command_separators: &['\n', '|', '&'],
    comments_with_hash: false,
};

/// Whether the platform's interpreter would rewrite this *word* before the
/// program word exists.
///
/// `cmd.exe` expands `%VAR%`, and its substring and replace forms
/// (`%VAR:~0,0%`, `%VAR:a=b%`), on the command line: `de%PATH:~0,0%l` is `del`
/// by the time anything runs. The pair is the signal, because a lone `%` is an
/// ordinary character to it.
///
/// Per word, not per line, and the difference is not academic: the line
/// `git log --format='%h %s' -5` carries two `%` in two different words, and
/// reading the line as a whole made that everyday command — with `git log`
/// default-trusted — ask on every Windows run. (It also turned an unguarded
/// assertion in `engine::tests` red under the Windows grammar, which is how the
/// cost was measured rather than argued.)
///
/// The residual the per-word reading leaves is a pair split by a blank, which
/// `cmd` reaches only through a *defined* variable: `%VAR:a=b%` accepts blanks
/// in the pattern, and a blank-named variable is settable (`setx "a b" …`).
/// An **undefined** `%X%` is left literal by `cmd`, so the split-pair spelling
/// is inert until someone has run the command that defines it — a command this
/// floor reads on its own terms.
#[must_use]
pub(super) fn rewrites_a_word(grammar: Grammar, word: &str) -> bool {
    if grammar.rewrites_words_with.is_empty() {
        return false;
    }
    word.chars()
        .filter(|ch| grammar.rewrites_words_with.contains(ch))
        .count()
        >= 2
}

/// Whether any word of `command` is one [`rewrites_a_word`] describes.
///
/// This is the trust side's question, and the only side that asks it: such a
/// line is never auto-approved and never a bounded edit, exactly as `$VAR` and
/// `$(…)` are not on Unix ([`has_shell_indirection`] folds it in). The deny
/// floor deliberately asks nothing about `%` — "no rule here can read this
/// word" is not "this line is catastrophic", and every attempt to make the
/// floor refuse it cost an ordinary command instead (`echo %PATH%`,
/// `%PYTHON% script.py`, `%COMSPEC% /c …` — the launcher shapes a model is
/// taught to write). It is an indirect form like any other.
#[must_use]
pub(super) fn rewrites_a_word_of_the_line(grammar: Grammar, command: &str) -> bool {
    command
        .split_whitespace()
        .any(|word| rewrites_a_word(grammar, word))
}

/// `command` with each of `delimiters` turned into a blank, or `None` when it
/// carries none of them and re-reading it would be redundant.
///
/// `cmd.exe` delimits words on `,` and `;` as well as blanks, so
/// `del,/f/s/q,C:\*` is a catastrophic command to it and one unrecognizable
/// word to a floor that splits on blanks — `basename_lower` of that word is
/// `*`, which matches no rule. Callers pass the set they may normalize
/// everywhere ([`Grammar::word_delimiters`]);
/// [`blanks_for_outside_flags`] is the same idea for a set that is only safe
/// to read between operands.
///
/// A caller re-reads for the same reason the deny floor re-reads a
/// brace-expanded line: the rewritten form is the one the interpreter really
/// runs, the raw line has already been checked, so this can only ever *add*
/// findings. That also makes it safe against the delimiter set being wider
/// than a given `cmd` build actually splits on.
///
/// The cost is one allocation and one more pass over the line, and only on a
/// line that carries one of the characters: `sh`'s set is empty, so `contains`
/// answers `None` before allocating and Unix pays nothing at all, while a
/// Windows caller is about to spawn a process.
#[must_use]
pub(super) fn blanks_for(grammar: Grammar, command: &str) -> Option<String> {
    let delimiters = grammar.word_delimiters;
    if !command.contains(delimiters) {
        return None;
    }
    Some(
        command
            .chars()
            .map(|ch| if delimiters.contains(&ch) { ' ' } else { ch })
            .collect(),
    )
}

/// `command` with each of `delimiters` turned into a blank *except* inside a
/// flag token — one whose first character is `-` — or `None` when the line
/// carries none of them.
///
/// This is [`Grammar::word_delimiters_outside_flags`]'s reading, and the carve
/// out is what makes it usable at all: `=` is one of `cmd`'s delimiters, but
/// `--flag=value` is far too common a shape to read as two words on a floor no
/// mode can override (`rm -r --exclude=/ build` became a recursive remove of
/// `/`). Outside a flag, the same character is what let `del=/f/s/q C:\*` past
/// every rule.
///
/// A replaced delimiter starts a new token, so the text after it is judged on
/// its own terms: `x=-rf` reads as `x -rf`, with `-rf` a flag token. Only `-`
/// opens a flag, not `cmd`'s `/switch` — a switch is already a word this floor
/// reads, and keeping the test to one character keeps the reading easy to state.
///
/// A token ends at a blank or at one of this grammar's
/// [`Grammar::command_separators`] — not at a blank alone, and under `cmd`
/// deliberately not at `;` or `,`; that field says why.
///
/// None of that reads a quoted character, and the quotes themselves open no
/// token. Both halves were bugs. A `&` inside a flag's value ended the flag
/// token, so `curl --data="user=x&su=1" https://h` read as `…&su 1"`, which
/// [`segments`] then cut at the `&` into a segment whose program word was `su`
/// — a privilege-escalation denial, on a floor no mode can override, for a line
/// `cmd` runs with the `&` as a literal. And a quote at the head of a token hid
/// the `-` behind it, so `"--exclude=/"` was no flag, its `=` was read, and
/// [`clean_token`] handed the rule a bare `/` operand: `rm -r "--exclude=/" build`
/// denied as a recursive remove of the root where the unquoted spelling was
/// fine. A delimiter *inside* the quotes is still read (`del"="/f/s/q C:\*` is
/// still `del /f/s/q C:\*`) — `cmd` removes the quotes before it splits, and
/// that spelling is the one this rewriting exists for.
///
/// The result may equal `command` when every `=` sits inside a flag
/// (`rm -r --exclude=/ build`). Whether that is worth judging again is the
/// enumeration's call, not this function's — `shell_deny::readings_of` keeps
/// each distinct reading once, for this rewriting and for every other — so the
/// contract here is the same as [`blanks_for`]'s: `None` exactly when the line
/// carries none of the delimiters.
#[must_use]
pub(super) fn blanks_for_outside_flags(grammar: Grammar, command: &str) -> Option<String> {
    let delimiters = grammar.word_delimiters_outside_flags;
    if !command.contains(delimiters) {
        return None;
    }
    let mut out = String::with_capacity(command.len());
    let mut token_start = true;
    let mut in_flag = false;
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        let quoted = quote.is_some();
        match quote {
            Some(active) if ch == active => quote = None,
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            _ => {}
        }
        if !quoted && (ch.is_whitespace() || grammar.command_separators.contains(&ch)) {
            token_start = true;
            out.push(ch);
            continue;
        }
        if token_start {
            if matches!(ch, '\'' | '"') {
                out.push(ch);
                continue;
            }
            in_flag = ch == '-';
            token_start = false;
        }
        if !in_flag && delimiters.contains(&ch) {
            out.push(' ');
            token_start = true;
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// The grammar of the host this build runs on — what every caller outside the
/// tests wants. The tests reach for [`SH`] and [`WINDOWS`] by name instead, so
/// both readings are pinned from any host.
pub(super) const HOST: Grammar = if cfg!(windows) { WINDOWS } else { SH };

/// Remove shell quoting from a single token so a deny/bounds check inspects
/// what `sh -c` will actually execute — not the raw, still-quoted text. Strips
/// every `'` and `"` (the shell removes quotes anywhere in a word, so `r""m`
/// runs as `rm` and `'-rf'` as `-rf`) plus the host's escape character; which
/// characters those are is [`Grammar::quoting_to_strip`].
///
/// This is a deliberate safety over-approximation: dropping these characters
/// can only *expose* a dangerous flag or path, never hide one, so it can never
/// weaken a deny rule or a workspace-bounds check. It MUST be applied to every
/// token a decision depends on — the program word AND its arguments — because a
/// check that cleaned only the program word (as an earlier version did) let
/// quoted flags like `rm '-rf' /` and quoted paths like `cp x '/tmp/out'` slip
/// straight past.
///
/// Shared by both sides: the trust matcher must strip the same quoting the deny
/// floor does, or a quoted redirecting flag (`--con"fig"`) that the shell runs
/// as `--config` rides a trusted identity the deny floor would have cleaned.
pub(super) fn clean_token(token: &str) -> String {
    clean_token_in(HOST, token)
}

/// [`clean_token`] under an explicit [`Grammar`], so the Windows text reading
/// (`de^l` is `del`, and a `\` is kept) is assertable from any host.
fn clean_token_in(grammar: Grammar, token: &str) -> String {
    token
        .chars()
        .filter(|ch| !grammar.quoting_to_strip.contains(ch))
        .collect()
}

/// The lowercased basename of a token, with shell quoting removed first, so
/// `/usr/bin/sudo`, `'sudo'`, and `s\udo` all resolve to `sudo`. What separates
/// components is [`Grammar::path_separators`]; on Unix a `\` was already
/// dropped by [`clean_token`].
pub(super) fn basename_lower(token: &str) -> String {
    basename_lower_in(HOST, token)
}

/// [`basename_lower`] under an explicit [`Grammar`] (see [`clean_token_in`]).
pub(super) fn basename_lower_in(grammar: Grammar, token: &str) -> String {
    let cleaned = clean_token_in(grammar, token);
    let base = cleaned
        .rsplit(grammar.path_separators)
        .next()
        .unwrap_or(cleaned.as_str())
        .to_ascii_lowercase();
    strip_executable_extension(&base)
}

/// Drop a Windows executable suffix so `powershell.exe` and `powershell`, or
/// `reg.exe` and `reg`, resolve to the same program word.
///
/// Unconditional rather than `cfg!(windows)` for the same reason the Windows
/// verb rules are: it keeps the floor testable from any host, and on Unix a
/// program genuinely named `rm.exe` is both vanishingly rare and safe to
/// over-approximate — this floor may only ever *expose* a dangerous name, never
/// hide one. Without it, `reg.exe delete`, `takeown.exe`, `diskpart.exe`,
/// `format.com` and `curl x | powershell.exe` all fell through to `_ => None`,
/// which is exactly how Windows documentation and scripts spell them.
fn strip_executable_extension(base: &str) -> String {
    const EXECUTABLE_SUFFIXES: &[&str] = &[".exe", ".com", ".bat", ".cmd"];
    for suffix in EXECUTABLE_SUFFIXES {
        if let Some(stem) = base.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return stem.to_string();
        }
    }
    base.to_string()
}

/// True if a command contains shell redirection, substitution, expansion or
/// grouping (`>`, `<`, `` ` ``, `$`, `{`/`}`, `(`/`)`, `*`, `?`, `[`/`]`). These
/// make the visible text an unreliable
/// description of what will run: a substitution executes an arbitrary inner
/// program (`touch $(curl …)`), a redirection writes a path no program word
/// mentions (`sed … > cfg`), and a `$VAR` expands to content the reviewer
/// never saw. Any such command is excluded from every automatic pass (trust
/// list, accept-edits) and goes to a human — which is what lets the deny floor
/// stay plain-form only instead of chasing obfuscations.
///
/// Brace expansion belongs on this list for exactly the same reason and was
/// the one word-expansion stage missing from it. Every check above this line
/// compares the *written* token against a rule — the redirecting-flag list,
/// the cwd-bounds spelling test, the program basename — and bash rewrites the
/// token before any of them describes what runs. `--con{fig,fig}` reaches
/// cargo as `--config`, so the default-trusted `cargo build` executed an
/// arbitrary program through `build.rustc-wrapper` with no prompt at any tier;
/// `git diff --no-index{,}` printed any file on the host into the transcript
/// the same way; `cp {~/.ssh/id_rsa,./k}` rode the accept-edits allowance,
/// whose operands "stay under the cwd by spelling" only while the spelling is
/// what the shell reads. Treating the punctuation as indirection is the same
/// over-approximation the rest of this list makes: a brace command is never
/// auto-trusted and never a bounded edit, so it lands on a human, and no
/// expander has to be right for the gate to be safe. (The deny floor does
/// expand them — see `shell_deny::builtin_deny` — because on the channels that
/// never parse a line it is the last rule that runs, and on Windows the only
/// thing behind them at all: see the `execution_policy` module map, "what is
/// behind a command nobody read".)
///
/// Both halves of a pair are listed because bash leaves an unbalanced brace
/// alone (`a{b` stays `a{b`): matching either character over-approximates in
/// the safe direction rather than requiring this to parse what bash will
/// expand.
///
/// Subshell parens are here rather than in one caller. `session_identity` used
/// to spell its own `contains(['(', ')'])` beside this call, which left the
/// module's stated "one view of the shell" invariant with a seam right at the
/// place a new construct has to be added — and a construct added to only one
/// of two lists is precisely how the brace hole stayed open. The other callers
/// already refused these commands for unrelated reasons (a peeled `(` is a
/// prefix word, and `(cargo` matches no rule), so folding it in changes no
/// verdict; it changes where the next construct has to be written.
///
/// Pathname expansion (`*`, `?`, `[…]`) is here for the same reason as braces,
/// and was the stage still missing after them: the pattern reaches the program
/// as whatever file names match it in the cwd, so `cargo build --con*` reached
/// cargo as `--config=build.rustc-wrapper=x` once a file of that name existed
/// (and the trusted `cargo test -- --logfile ./<name>` creates a file of any
/// name with no prompt), while `git diff --no-inde? …` reopened the `--no-index`
/// read the flag list had closed. The cost is that `git diff -- src/*.rs` asks.
/// Which characters belong here is no longer the author's call:
/// `every_punctuation_the_shell_rewrites_is_accounted_for` asks the real shell
/// which characters rewrite a word and fails naming any that no rule reads.
///
/// Takes the [`Grammar`] rather than reading [`HOST`]: the shared list above is
/// every platform's, and [`Grammar::rewrites_words_with`] adds the platform's
/// own — `%` on Windows, where `cmd.exe` rewrote `de%PATH:~0,0%l` into `del`
/// after every rule here had read the word. One function rather than a host
/// wrapper, because no caller wants the host's reading implicitly.
#[must_use]
pub(super) fn has_shell_indirection(grammar: Grammar, command: &str) -> bool {
    command.contains(['>', '<', '`', '$', '{', '}', '(', ')', '*', '?', '[', ']'])
        || rewrites_a_word_of_the_line(grammar, command)
}

/// Whether a (cleaned) token names a path that leaves the current directory by
/// its spelling alone: absolute (`/etc/x`, `\x`), home-relative (`~/.ssh`,
/// `~user`), drive-lettered (`C:\x`) or climbing through a `..` path component
/// (`../x`, `a/../b`, `..`). `..` counts only as a whole component: `main..HEAD`
/// is a revision range and `my..dir` a file name, and reading the two dots
/// anywhere in the word turned `git diff main..HEAD` into a prompt.
///
/// The sandbox bounds the *write* side of such a path; the *read* side it
/// leaves open (`(allow file-read*)`), so this spelling is the read fence — for
/// the accept-edits allowance (`cp ~/.ssh/id_rsa ./k`), for the trust gate and
/// the session key (`git diff /dev/null ~/.ssh/id_rsa` on the default-trusted
/// `git diff`), and for the safety notes that warn the human. One predicate, so
/// the three never disagree about what "outside" looks like.
///
/// Deliberately platform-blind, unlike [`Grammar`]: a `\` counts as a separator
/// and a leading one as absolute on every host. This fence may only ever
/// over-refuse — reading a Unix file named `..\x` as a climb costs a prompt,
/// while reading `..\secret` as an ordinary name on the platform where it *is*
/// a climb costs the file. Do not "unify" it with the grammar above.
pub(super) fn escapes_cwd_by_spelling(token: &str) -> bool {
    let bytes = token.as_bytes();
    token.starts_with(['/', '~', '\\'])
        || token.split(['/', '\\']).any(|component| component == "..")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Whether an argument token (quoting already stripped) names a path outside
/// the cwd: a positional operand judged as is, a flag's `=value` judged by its
/// value (`--target-directory=/tmp` is a target like any other), a *short*
/// flag's glued value judged by [`glued_value_leaves_cwd`], a bare flag
/// (`-r`, `--`) never. Judging only the words that do not start with `-` let
/// the `=value` spelling of a target through while `-t /tmp` was refused.
///
/// The glued reading is the half that was missing, and it was a hole in every
/// promise built on this fence rather than a rough edge: `getopt` lets a short
/// option carry its value with no separator at all, so `cp -t/tmp ./x` is the
/// same command as `cp -t /tmp ./x`, and the `=`-only reading returned `false`
/// for it. That made `mv -t/tmp src` a *bounded edit* to
/// [`super::shell_deny::is_workspace_fs_edit`] — AcceptEdits and Auto ran it
/// with no prompt, and `/tmp` is a sandbox write root by design, so nothing
/// below this line stopped it either — and it let the same spelling ride any
/// configured trust rule whose program takes such an option (`tar -C/etc …`).
///
/// Only a single `-` opens a glued value. A long flag spells its value after
/// `=` or in the next word; it never glues, so reading `--exclude-from` for a
/// value would cost prompts for nothing.
pub(super) fn operand_leaves_cwd(cleaned: &str) -> bool {
    let Some(flag) = cleaned.strip_prefix('-') else {
        return escapes_cwd_by_spelling(cleaned);
    };
    // A flag's value after `=`, judged exactly — so an in-tree target keeps
    // its allowance (`cp --target-directory=build/out x`). The name before the
    // `=` must not itself be a path, or a glued value that happens to carry an
    // `=` (`-t/tmp=x`) would be read as the flag `t/tmp` with the value `x`.
    if let Some((name, value)) = flag.split_once('=')
        && !name.contains(['/', '\\', '~'])
    {
        return escapes_cwd_by_spelling(value);
    }
    !flag.starts_with('-') && glued_value_leaves_cwd(flag)
}

/// Whether any suffix of a short-flag bundle (the token past its single `-`)
/// leaves the cwd by spelling — i.e. whether the value glued onto it does,
/// wherever the program decides its option letters end.
///
/// Where they end is the *program's* business and this fence does not know it:
/// `-t/tmp` is one option letter, `-rft/tmp` is three, and a bundle is free to
/// be longer still. So every position the value could start at is judged, and
/// the token is refused if any of them escapes.
///
/// Written as the three conditions rather than as a loop over suffixes,
/// because the loop is quadratic in the token and this runs on every argument
/// of every shell call — a model can emit a 100 KB flag token. They are the
/// same predicate, term by term against [`escapes_cwd_by_spelling`]:
///
///   * a suffix starting with `/`, `~` or `\` exists exactly when the bundle
///     holds one of those characters anywhere;
///   * a suffix with a `..` *component* needs a separator, which the first
///     term already caught, unless the suffix IS `..` — so, with no separator
///     present, exactly when the bundle ends with `..`;
///   * a drive-lettered suffix exists exactly when some letter is followed by
///     a `:`. Byte pairs are enough: a UTF-8 continuation byte is never
///     `is_ascii_alphabetic`, and `:` never appears inside a multi-byte
///     sequence.
///
/// `every_glued_value_reading_matches_the_suffix_scan` holds the two together.
///
/// The cost is that an in-tree path glued to a short flag (`cp -tbuild/out x`)
/// is refused as well: this fence cannot tell it from `-t/tmp` without knowing
/// where the letters end, and it may only ever over-refuse. That is one
/// prompt, on a spelling a model rarely writes.
fn glued_value_leaves_cwd(bundle: &str) -> bool {
    bundle.contains(['/', '\\', '~'])
        || bundle.ends_with("..")
        || bundle
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0].is_ascii_alphabetic() && pair[1] == b':')
}

/// The words `command` runs as, when it is one simple command the executor
/// could run unattended: the argv [`parse_unattended`] produced, program word
/// first. `None` for anything else — a sequence, or a line the executor would
/// refuse.
///
/// This is what every rule that judges *arguments* must read. The alternative
/// reading, [`clean_token`] over whitespace-split tokens, is a second grammar
/// for the same text, and the two are equal only by accident: it deletes
/// quotes wherever they sit, so `'--'` stayed a quoted word to the rules while
/// the executor passed a real `--` to the program, and it keeps a Windows `\`
/// the parser eats. Both readings existed because the rules were written when
/// the text went to `sh -c`; the executor now runs this argv, so the rules read
/// it too.
#[must_use]
pub(super) fn executed_words(command: &str) -> Option<Vec<String>> {
    executed_words_in(HOST, command)
}

/// [`executed_words`] under an explicit [`Grammar`] (see
/// [`parse_unattended_in`]).
#[must_use]
pub(super) fn executed_words_in(grammar: Grammar, command: &str) -> Option<Vec<String>> {
    let mut commands = parse_unattended_in(grammar, command)?;
    if commands.len() != 1 {
        return None;
    }
    Some(commands.pop()?.argv)
}

/// How a command in an unattended sequence is gated on the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunIf {
    /// Runs regardless: the first command, or one after `;` or a newline.
    Always,
    /// Runs only if the previous command exited 0 (`&&`).
    PreviousSucceeded,
}

/// One simple command of an unattended sequence: the exact argv the executor
/// hands to `execve`, and whether it is gated on the previous command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedCommand {
    /// The words handed to `execve`, program word first — the argv every rule
    /// judges (see [`executed_words`]) and the argv that runs.
    pub argv: Vec<String>,
    /// Whether the command runs regardless or only after the previous one
    /// succeeded.
    pub run_if: RunIf,
}

/// The argv sequence of a command line that may run *unattended* — on the
/// strength of the policy's own parse, with no human reading the text — or
/// `None` when the line uses anything a shell would have to interpret.
///
/// This is the other half of the trust gate's promise. Every rule in this
/// module and in `command_shape` judges the *written* words; until now the
/// words then went to `sh -c`, which reads them by its own grammar, and every
/// difference between the two grammars was a way to run something the gate
/// never saw — quote splicing, brace expansion, globbing, an implicit
/// `--no-index`: ten review rounds of them, one table entry at a time. A
/// command that clears the gate is now executed as the argv produced here, so
/// the words the gate judged are the words that run, by construction rather
/// than by the completeness of any list.
///
/// Accepted grammar — what this parser reads, which is also the only reading
/// that runs (the argv it returns is what reaches the OS): words split on
/// unquoted blanks; `'…'` literal; `"…"` literal, with `\"` and `\\` escapes
/// where [`Grammar::backslash_escapes`] holds; `#` opening a word comments out
/// the rest of the line; commands chain by `;`, a newline, or `&&`. Everything
/// else is `None` — `|`, `||`, a lone `&`, redirection, substitution, any
/// expansion ([`has_shell_indirection`]), an unterminated quote, an empty
/// command, a `~`-led word (the shell would expand it; the gate refuses such an
/// operand anyway), a program word carrying `=` (an assignment prefix) — so the
/// gate does not auto-approve it and the executor does not run it unattended.
///
/// Under [`SH`] this subset means exactly what that shell means by it, checked
/// against the real one by `unattended_parse_matches_sh_word_splitting`. Under
/// [`WINDOWS`] there is no shell to agree with — `cmd.exe` is not in the path
/// of an unattended command — and two of the rules above are this parser's own
/// uniform choice rather than a platform fact: `'…'` quoting (`cmd.exe` has no
/// `'`) and quote removal. That is sound because the words produced here are
/// the argv that runs; it is *not* a claim that `CommandLineToArgvW` would read
/// the same text the same way. Where that difference could move a word
/// boundary, the line is refused instead ([`ambiguous_backslash_quote`]).
///
/// Backslashes are the one place the two platforms read the same text
/// differently, and which reading applies is [`Grammar::backslash_escapes`]
/// rather than a `cfg!` here: under [`SH`] a `\` escapes the next character and
/// a `\`-newline continues the line (`unattended_parse_matches_sh_word_splitting`
/// checks that against the real shell), while under [`WINDOWS`] it is an
/// ordinary path character, so `git diff src\main.rs` keeps its path and `echo
/// done\` is a finished line rather than an unfinished one. Both readings are
/// pinned from every host by `backslash_reading_is_pinned_for_both_grammars`.
#[must_use]
pub fn parse_unattended(command: &str) -> Option<Vec<UnattendedCommand>> {
    parse_unattended_in(HOST, command)
}

/// Whether the line spells a backslash immediately before a `"` — the one
/// spelling Windows argument parsers disagree about, and so the one this
/// parser refuses to read rather than guess at.
///
/// `CommandLineToArgvW` counts the backslash run before a quote, so `\"` is a
/// literal quote to the program while `\\"` is a backslash plus a real quote,
/// and `cmd.exe` counts it differently again. Refusing costs a prompt and
/// keeps both sides of the fence reading the same word.
///
/// A property of the *line*, tested once, rather than of a parse position
/// tested wherever a quote is consumed — and that is the whole point. The
/// per-position version reached the top-level arm and the double-quote loop
/// and missed the single-quote loop, so the same backslash was refused outside
/// quotes (`echo \'x'`) and read as an ordinary character inside them
/// (`echo 'a\"b'` came out as the one word `a\"b`): the reading depended on
/// which arm consumed it rather than on the spelling. That second line is
/// exactly what the line-level test changed — it is refused now, like every
/// other `\"` — and a line-level test has no position for the next arm to
/// dodge it from.
///
/// `""` inside a word is deliberately *not* refused, though it is the same
/// kind of disputed spelling: `CommandLineToArgvW` documents `""` inside a
/// quoted block as an escaped literal quote, while this parser reads it as
/// close-then-reopen, so `git commit -m "say ""hi"" ok"` is `say hi ok` here
/// and `say "hi" ok` through `cmd /C`. It stays because that difference can
/// only add or drop quote characters *inside* a word — the word count is the
/// same, so no flag becomes an operand and no path changes — whereas `\"` can
/// move a boundary. Pinned by
/// `windows_reads_doubled_quotes_as_reopening_the_word`.
///
/// `'` is deliberately not here, though the per-position version refused it
/// too: it is not special to `CommandLineToArgvW`, to `CreateProcess`, or to
/// `cmd.exe`, so `\'` has exactly one Windows reading and refusing it only
/// cost a prompt on ordinary text (`git commit -m "don\'t ship"`). Quote
/// removal is this parser's own rule, applied the same way on both platforms
/// whether or not a backslash precedes the quote.
///
/// Only consulted where [`Grammar::backslash_escapes`] is false. Under [`SH`]
/// the escape arm claims the backslash first and the real shell agrees with it
/// (`unattended_parse_matches_sh_word_splitting`), so `printf 'a\"b'` keeps
/// working there.
fn ambiguous_backslash_quote(command: &str) -> bool {
    command.contains(r#"\""#)
}

/// [`parse_unattended`] under an explicit [`Grammar`], so both platforms'
/// readings of the same line are reachable from any host.
fn parse_unattended_in(grammar: Grammar, command: &str) -> Option<Vec<UnattendedCommand>> {
    if has_shell_indirection(grammar, command) {
        return None;
    }
    if !grammar.backslash_escapes && ambiguous_backslash_quote(command) {
        return None;
    }
    let mut commands: Vec<UnattendedCommand> = Vec::new();
    let mut argv: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    // How the command being collected is gated: set by the separator that
    // opened it, `Always` for the first.
    let mut gate = RunIf::Always;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' if grammar.backslash_escapes => match chars.next() {
                // A trailing backslash is an incomplete line to the shell.
                None => return None,
                Some('\n') => {}
                Some(next) => word.get_or_insert_with(String::new).push(next),
            },
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        None => return None,
                        Some('\'') => break,
                        Some(ch) => w.push(ch),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        None => return None,
                        Some('"') => break,
                        Some('\\') if grammar.backslash_escapes => match chars.next() {
                            None => return None,
                            Some(escaped @ ('"' | '\\')) => w.push(escaped),
                            Some('\n') => {}
                            Some(other) => {
                                w.push('\\');
                                w.push(other);
                            }
                        },
                        Some(ch) => w.push(ch),
                    }
                }
            }
            ' ' | '\t' => {
                if let Some(w) = word.take() {
                    argv.push(w);
                }
            }
            '\n' | ';' => {
                if let Some(w) = word.take() {
                    argv.push(w);
                }
                if argv.is_empty() {
                    // `;` with nothing before it is a syntax error to the
                    // shell; a blank line is not.
                    if c == ';' {
                        return None;
                    }
                    continue;
                }
                commands.push(UnattendedCommand {
                    argv: std::mem::take(&mut argv),
                    run_if: gate,
                });
                gate = RunIf::Always;
            }
            '&' => {
                chars.next_if_eq(&'&')?;
                if let Some(w) = word.take() {
                    argv.push(w);
                }
                if argv.is_empty() {
                    return None;
                }
                commands.push(UnattendedCommand {
                    argv: std::mem::take(&mut argv),
                    run_if: gate,
                });
                gate = RunIf::PreviousSucceeded;
            }
            '|' => return None,
            '#' if grammar.comments_with_hash && word.is_none() => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            other => word.get_or_insert_with(String::new).push(other),
        }
    }
    if let Some(w) = word.take() {
        argv.push(w);
    }
    if !argv.is_empty() {
        commands.push(UnattendedCommand { argv, run_if: gate });
    } else if gate == RunIf::PreviousSucceeded {
        // `a &&` with nothing after it.
        return None;
    }
    if commands.is_empty() {
        return None;
    }
    for command in &commands {
        let program = command.argv.first()?;
        if program.is_empty() || program.contains('=') {
            return None;
        }
        if command.argv.iter().any(|word| word.starts_with('~')) {
            return None;
        }
    }
    Some(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Candidate words that might hand execution to the rest of the line.
    /// Deliberately broader than either list above — the point is that the
    /// SHELL decides which of these belong, not the author.
    ///
    /// Absent on purpose: `sudo`/`su`/`doas` (the deny floor refuses them
    /// outright, and a host with passwordless sudo would run the recorder),
    /// and `ssh`/`watch`-style words are covered but bounded by the deadline
    /// below because they never exit on their own.
    #[cfg(unix)]
    const WRAPPER_CANDIDATES: &[&str] = &[
        "eval",
        "source",
        ".",
        "exec",
        "command",
        "builtin",
        "env",
        "time",
        "nohup",
        "nice",
        "setsid",
        "stdbuf",
        "timeout",
        "ionice",
        "taskset",
        "unshare",
        "chroot",
        "flock",
        "caffeinate",
        "xcrun",
        "arch",
        "watch",
        "script",
        "proxychains",
        "parallel",
        "xargs",
        "busybox",
        "runuser",
        "sh",
        "bash",
        "zsh",
        "dash",
        "ksh",
        "fish",
        "perl",
        "python",
        "python3",
        "ruby",
        "node",
        "php",
        "awk",
        "gawk",
        "osascript",
        "tclsh",
        "lua",
        "julia",
        "Rscript",
        "deno",
        "bun",
        "if",
        "then",
        "else",
        "while",
        "do",
        "!",
    ];

    #[test]
    fn basename_lower_strips_paths_quotes_and_executable_suffixes() {
        assert_eq!(basename_lower("powershell.exe"), "powershell");
        assert_eq!(basename_lower("C:/Windows/System32/cmd.exe"), "cmd");
        assert_eq!(basename_lower("REG.EXE"), "reg");
        assert_eq!(basename_lower("format.com"), "format");
        assert_eq!(basename_lower("takeown.exe"), "takeown");
        // A dot that is not an executable suffix stays part of the name.
        assert_eq!(basename_lower("my.script"), "my.script");
        // A bare suffix is a real (if odd) filename, not an empty stem.
        assert_eq!(basename_lower(".exe"), ".exe");
    }
    /// Whether `sh` hands the rest of the line to the recorder when the line
    /// opens with `word`. Bounded: some candidates (`watch`) never exit.
    #[cfg(unix)]
    fn shell_runs_the_tail(word: &str, index: usize, dir: &std::path::Path) -> bool {
        use std::io::Write;
        let marker = dir.join(format!("ran{index}"));
        let recorder = dir.join(format!("rec{index}"));
        let mut file = std::fs::File::create(&recorder).unwrap();
        writeln!(file, "#!/bin/sh\n: > {}", marker.display()).unwrap();
        drop(file);
        std::fs::set_permissions(
            &recorder,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let Ok(mut child) = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{word} {}", recorder.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        marker.exists()
    }

    /// The completeness property the two lists above exist to satisfy, checked
    /// against the real shell instead of against the author's memory.
    ///
    /// If `sh` runs the rest of the line, the identity side MUST NOT collapse
    /// that line to the bare word — otherwise one session "a" on
    /// `source .venv/bin/activate` silently covers `source ./anything.sh` for
    /// the rest of the session. Hand-maintained lists kept missing members of
    /// this class one round at a time (`sh`/`time` one release, `eval`/
    /// `source`/`.` the next); this test enumerates the class instead.
    ///
    /// It is one-directional on purpose: a word in the lists that this host's
    /// shell does not treat as a wrapper is a harmless over-approximation
    /// (`busybox` and the Windows interpreters are not installed here), so
    /// only the unsafe direction is asserted.
    #[cfg(unix)]
    #[test]
    fn every_word_the_shell_runs_the_tail_for_is_known() {
        let dir = std::env::temp_dir().join(format!("dc-wrapper-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut missing = Vec::new();
        for (index, word) in WRAPPER_CANDIDATES.iter().enumerate() {
            if shell_runs_the_tail(word, index, &dir)
                && !runs_the_rest_of_the_line(&basename_lower(word))
            {
                missing.push(*word);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            missing.is_empty(),
            "the shell hands the rest of the line to these words, but the identity side still \
             collapses such a line to the bare word — a session consent on one of them would \
             cover every command reachable through it: {missing:?}"
        );
    }

    /// The rules that read a character the shell rewrites a word for (see the
    /// test below): expansion, substitution, redirection and grouping are
    /// indirection; `;`/`|`/`&` split segments; quotes and the backslash are
    /// stripped by `clean_token`; a `~`-led operand is refused by
    /// `escapes_cwd_by_spelling`; and `#` only ever makes the shell run a
    /// *prefix* of what the gate read — the safe direction, so it needs no rule.
    /// Unix-only like the test that reads it: on Windows the pair is dead code
    /// and `-D warnings` refuses to compile it.
    #[cfg(unix)]
    fn rewriting_character_is_read(c: char) -> bool {
        has_shell_indirection(HOST, &c.to_string())
            || SEGMENT_SEPARATORS.contains(&c)
            || matches!(c, '\'' | '"' | '\\')
            || c == '~'
            || c == '#'
    }

    /// The characters the shell rewrites a word for, enumerated against the
    /// real shell instead of remembered. For every ASCII punctuation character,
    /// words built around it — alone, at either end, in the middle, doubled
    /// (`` `a` ``, `'a'`, `$a$`), and closed by `}` or `]` (`a{x,y}`, `a[x,y]`,
    /// `a{1..3}`) — are handed to `sh -c 'printf "%s\n" …'` in a directory seeded
    /// with names a pattern can match. If what comes back is not the words as
    /// written — expanded, split, dropped, or a failed parse — that character
    /// rewrites text, and some rule must read it (`rewriting_character_is_read`).
    /// Only `}` and `]` serve as closers because they are the two the shell
    /// leaves alone when unpaired: any other closer would break the batch on
    /// its own and charge the fault to the character under test.
    ///
    /// Hand-maintained, the indirection list missed one stage per review round:
    /// quote removal, then brace expansion, then pathname expansion. The
    /// alphabet is finite, so this is enumeration, not sampling: a
    /// single-character trigger no rule reads cannot exist on this host without
    /// failing here. One-directional like the wrapper test above — a listed
    /// character the host's shell does not rewrite (dash and braces) only ever
    /// costs a prompt.
    #[cfg(unix)]
    #[test]
    fn every_punctuation_the_shell_rewrites_is_accounted_for() {
        let dir = std::env::temp_dir().join(format!("dc-punct-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["ax", "ay", "a1", "ab"] {
            std::fs::File::create(dir.join(name)).unwrap();
        }
        let punctuation: Vec<char> = (b'!'..=b'~')
            .filter(u8::is_ascii_punctuation)
            .map(char::from)
            .collect();
        let mut unaccounted = Vec::new();
        for &c in &punctuation {
            let mut words = vec![
                c.to_string(),
                format!("a{c}"),
                format!("{c}a"),
                format!("a{c}b"),
                format!("{c}a{c}"),
            ];
            for d in ['}', ']', c] {
                words.push(format!("a{c}x,y{d}"));
                words.push(format!("a{c}1..3{d}"));
            }
            let script = format!("printf '%s\\n' {}", words.join(" "));
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .current_dir(&dir)
                .env("HOME", &dir)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .expect("run sh");
            let unchanged = output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .eq(words.iter().map(String::as_str));
            if !unchanged && !rewriting_character_is_read(c) {
                unaccounted.push(c);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            unaccounted.is_empty(),
            "the shell rewrites a word around these characters and no rule reads them — a \
             trusted or accept-edits command spelled with one runs as something the gate never \
             saw: {unaccounted:?}"
        );
    }

    fn argvs(command: &str) -> Vec<Vec<String>> {
        parse_unattended(command)
            .expect("parses")
            .into_iter()
            .map(|cmd| cmd.argv)
            .collect()
    }

    fn words(argvs: &[&[&str]]) -> Vec<Vec<String>> {
        argvs
            .iter()
            .map(|argv| argv.iter().map(|word| (*word).to_string()).collect())
            .collect()
    }

    fn gates(command: &str) -> Vec<RunIf> {
        parse_unattended(command)
            .expect("parses")
            .iter()
            .map(|cmd| cmd.run_if)
            .collect()
    }

    fn argvs_in(grammar: Grammar, command: &str) -> Option<Vec<Vec<String>>> {
        parse_unattended_in(grammar, command)
            .map(|cmds| cmds.into_iter().map(|cmd| cmd.argv).collect())
    }

    /// How a `\` reads is the one thing the two platforms do differently with
    /// the same text, so both readings are pinned here, as a table, from every
    /// host.
    ///
    /// The shape matters as much as the cases. This was two `#[cfg]` blocks,
    /// which meant each platform's half only ever ran on that platform: the
    /// Windows half was written blind and Windows CI was the first thing to
    /// execute it (it was wrong, twice), and on a target that is neither the
    /// body vanished entirely while the parser still had a reading. A table
    /// over [`SH`] and [`WINDOWS`] runs both halves everywhere; only the
    /// differential test against the real shell has to stay `cfg(unix)`,
    /// because it needs a real `sh`.
    ///
    /// Under [`SH`] a `\` escapes, which is what that shell does. Under
    /// [`WINDOWS`] it is an ordinary path character: reading it as an escape
    /// there ran `git diff src\main.rs` as `git diff srcmain.rs` on the
    /// default-trusted `git diff`, and collapsed `.\.` — the current
    /// directory — into `..`, its parent, a path the operand fence would have
    /// refused.
    #[test]
    fn backslash_reading_is_pinned_for_both_grammars() {
        /// One row: a grammar, a line, and the argv the executor would run
        /// under that grammar — `None` when the line is not one this parser
        /// will run unattended at all. Declared inside the function on
        /// purpose: an item between a doc comment and the item it describes
        /// re-parents the prose, which is the mistake this module already made
        /// once (see the header) and made again here, in the very commit that
        /// added the lint against it.
        type BackslashCase<'a> = (Grammar, &'a str, Option<&'a [&'a [&'a str]]>);

        let cases: &[BackslashCase<'_>] = &[
            // `sh`: a `\` escapes a path separator, a blank, a quote, itself,
            // and a newline (which continues the line); a trailing one leaves
            // a line only a shell could finish.
            (
                SH,
                r"git diff src\main.rs",
                Some(&[&["git", "diff", "srcmain.rs"]]),
            ),
            (SH, r"mkdir .\.", Some(&[&["mkdir", ".."]])),
            (SH, r"cargo test e\ f", Some(&[&["cargo", "test", "e f"]])),
            (
                SH,
                "cargo build \\\n  --release\n\n",
                Some(&[&["cargo", "build", "--release"]]),
            ),
            (SH, "echo done\\", None),
            (
                SH,
                "git commit -m \"say \\\"hi\\\" \\\\ ok\"",
                Some(&[&["git", "commit", "-m", "say \"hi\" \\ ok"]]),
            ),
            // Inside `'…'` nothing escapes, on either platform: `sh` reads
            // `'a\'b'c'` as the single word `a\bc`, and so does this parser.
            (SH, r"echo 'a\'b'c'", Some(&[&["echo", r"a\bc"]])),
            (SH, r#"echo 'a\"b'"#, Some(&[&["echo", r#"a\"b"#]])),
            // `\'` escapes the quote, so the word after it is unterminated.
            (SH, r"echo \'x'", None),
            // Windows: the same backslashes are path characters, so a path
            // keeps its spelling and a trailing one is a finished line.
            (
                WINDOWS,
                r"git diff src\main.rs",
                Some(&[&["git", "diff", r"src\main.rs"]]),
            ),
            (WINDOWS, r"mkdir .\.", Some(&[&["mkdir", r".\."]])),
            (
                WINDOWS,
                r"cargo test e\ f",
                Some(&[&["cargo", "test", r"e\", "f"]]),
            ),
            (WINDOWS, r"mkdir src\", Some(&[&["mkdir", r"src\"]])),
            (WINDOWS, "echo done\\", Some(&[&["echo", "done\\"]])),
            // Hugging a quote is the one spelling Windows argument parsers
            // disagree about, so it is refused rather than guessed at.
            (WINDOWS, "git commit -m \"say \\\"hi\\\" ok\"", None),
            (WINDOWS, r#"cd "C:\Users\me\""#, None),
            // `\'` is not one of them: `'` is not special to any Windows
            // argument parser, so the backslash is just a backslash and the
            // quote is removed the same way it is everywhere else.
            (WINDOWS, r"echo \'x'", Some(&[&["echo", r"\x"]])),
            (
                WINDOWS,
                r#"git commit -m "don\'t ship""#,
                Some(&[&["git", "commit", "-m", r"don\'t ship"]]),
            ),
            (WINDOWS, r"echo 'a\'b'c'", Some(&[&["echo", r"a\bc"]])),
            // Refused for the same reason as the others, from the same one
            // test: a `\"` inside `'…'` is where the per-arm version had no
            // check at all.
            (WINDOWS, r#"echo 'a\"b'"#, None),
        ];
        for &(grammar, line, expected) in cases {
            assert_eq!(
                argvs_in(grammar, line),
                expected.map(words),
                "{line:?} under {grammar:?}"
            );
        }

        // The fence that judges those words agrees with each reading about
        // what the word IS: `..` climbs, `.\.` is the cwd it spells, and
        // `done\` stays put.
        assert!(operand_leaves_cwd(".."));
        assert!(!operand_leaves_cwd(r".\."));
        assert!(!operand_leaves_cwd(r"done\"));

        // And production reads the host's grammar — a table both halves of
        // which pass proves nothing if the entry point reaches for neither.
        assert_eq!(HOST, if cfg!(windows) { WINDOWS } else { SH });
        assert_eq!(
            parse_unattended(r"git diff src\main.rs"),
            parse_unattended_in(HOST, r"git diff src\main.rs")
        );
    }

    /// Enumerated rather than sampled: wherever a `\"` lands in a line, the
    /// Windows reading refuses that line. Sampling is exactly what shipped the
    /// per-arm version with the single-quote arm missing — every example anyone
    /// wrote happened to land outside `'…'`.
    #[test]
    fn windows_refuses_a_backslash_before_a_quote_wherever_it_lands() {
        for base in [
            "cargo test --features \"a b\"",
            "git commit -m 'msg'",
            r"git diff src\main.rs",
            "echo done # note",
            "cargo build && cargo test",
        ] {
            // `0..=len`, not `char_indices()`: the end of the line is a
            // landing position too, and `char_indices()` never yields it.
            //
            // What the enumeration buys is position-independence and nothing
            // more — the check is a `contains` over the whole line, so it
            // cannot depend on where the spelling lands, and any line-level
            // implementation passes this. It is a guard against the *shape*
            // the per-arm version had, not a description of the refusal set;
            // which spellings are refused is the table above.
            for at in (0..=base.len()).filter(|at| base.is_char_boundary(*at)) {
                let line = format!("{}{}{}", &base[..at], r#"\""#, &base[at..]);
                assert_eq!(
                    parse_unattended_in(WINDOWS, &line),
                    None,
                    "{line:?} spells a backslash before a quote and must not run \
                     unattended under the Windows grammar"
                );
            }
        }
    }

    /// `""` inside a word reopens the word here, while `CommandLineToArgvW`
    /// documents it as an escaped literal quote — a real difference between
    /// this parser and that one, left in on purpose (see
    /// [`ambiguous_backslash_quote`]) and therefore written down.
    #[test]
    fn windows_reads_doubled_quotes_as_reopening_the_word() {
        let doubled = "git commit -m \"say \"\"hi\"\" ok\"";
        assert_eq!(
            executed_words_in(WINDOWS, doubled),
            Some(vec![
                "git".to_string(),
                "commit".to_string(),
                "-m".to_string(),
                "say hi ok".to_string(),
            ])
        );
        // Why it is tolerable where `\"` is not: quote characters come and go
        // inside the word, but the word count does not move, so no flag turns
        // into an operand and no path changes.
        assert_eq!(
            executed_words_in(WINDOWS, doubled).map(|words| words.len()),
            executed_words_in(WINDOWS, "git commit -m \"say hi ok\"").map(|words| words.len())
        );
    }

    /// `#` opens a comment in `sh`. `cmd.exe` has no comment syntax and `#` is
    /// a legal filename character there, so reading it as one deleted operands
    /// from the argv the executor runs — on a line the gate had already
    /// trusted, because the program word was still `git log`.
    #[test]
    fn hash_opens_a_comment_only_where_the_interpreter_has_comments() {
        assert_eq!(
            argvs_in(SH, "cargo build # not run"),
            Some(words(&[&["cargo", "build"]]))
        );
        assert_eq!(
            argvs_in(WINDOWS, "cargo build # not run"),
            Some(words(&[&["cargo", "build", "#", "not", "run"]]))
        );
        assert_eq!(
            argvs_in(WINDOWS, "git log --grep #123"),
            Some(words(&[&["git", "log", "--grep", "#123"]]))
        );
        assert_eq!(
            argvs_in(WINDOWS, "type #notes.md"),
            Some(words(&[&["type", "#notes.md"]]))
        );
        // Glued to a word it is an ordinary character on both.
        assert_eq!(
            argvs_in(SH, "cargo build#x"),
            Some(words(&[&["cargo", "build#x"]]))
        );
        assert_eq!(
            argvs_in(WINDOWS, "cargo build#x"),
            Some(words(&[&["cargo", "build#x"]]))
        );
    }

    /// `%` is how `cmd.exe` rewrites a line, so it is indirection under
    /// [`WINDOWS`] and an ordinary character under [`SH`] — where two of them
    /// in one word is an everyday command and refusing it would buy nothing.
    #[test]
    fn percent_is_indirection_only_where_the_interpreter_expands_it() {
        assert!(has_shell_indirection(WINDOWS, "echo %PATH%"));
        assert_eq!(
            parse_unattended_in(WINDOWS, r"de%PATH:~0,0%l /f/s/q C:\*"),
            None
        );
        // Per word: this everyday command carries two `%` in two words and
        // must stay readable — reading the line as a whole made `git log`,
        // which is default-trusted, ask on every Windows run.
        assert!(!has_shell_indirection(
            WINDOWS,
            r"git log --format='%h %s' -5"
        ));
        assert!(!rewrites_a_word(WINDOWS, "--format='%h"));
        assert!(rewrites_a_word(WINDOWS, "%PATH%"));
        // A lone `%` is not a rewrite: `cmd` needs the pair.
        assert!(!has_shell_indirection(WINDOWS, "git log --format=%h -5"));
        // `sh` has no such rewrite, so none of this costs a Unix command
        // anything, however many `%` it carries.
        assert!(!has_shell_indirection(SH, "date +%Y%m%d"));
        assert!(!rewrites_a_word(SH, "%Y%m%d"));
        assert_eq!(
            argvs_in(SH, "git log --format=%h%s -5"),
            Some(words(&[&["git", "log", "--format=%h%s", "-5"]]))
        );
        // cmd's other delimiters, which the deny floor re-reads through. `;` is
        // one of them: it delimits words for `cmd` exactly as `,` does, and the
        // merge that makes of a line this floor segments is `cmd`'s own reading
        // of it.
        for spelled in [r"del,/f/s/q,C:\*", r"del;/f/s/q;C:\*"] {
            assert_eq!(
                blanks_for(WINDOWS, spelled).as_deref(),
                Some(r"del /f/s/q C:\*")
            );
            assert_eq!(blanks_for(SH, spelled), None);
        }
        // `=` is read only where it cannot be a flag's value. Both halves are
        // load-bearing: the first spelling walked past every rule on the floor,
        // and reading the second one as two words denied an everyday command in
        // every mode with no way to say yes.
        assert_eq!(
            blanks_for_outside_flags(WINDOWS, r"del=/f/s/q C:\*").as_deref(),
            Some(r"del /f/s/q C:\*")
        );
        // Both in one line, one blanked and one kept: a `None` for the
        // all-inside-a-flag line could not tell the carve-out preserving an
        // `=` from the token model losing the delimiter.
        assert_eq!(
            blanks_for_outside_flags(WINDOWS, "del=/f --exclude=/ x").as_deref(),
            Some("del /f --exclude=/ x")
        );
        // Every `=` inside a flag: the line comes back unchanged, as `Some`.
        // Whether an unchanged reading is worth judging again is decided once,
        // in `shell_deny::readings_of`, for this rewriting and every other; a
        // `None` here would be the same decision made a second time in one
        // rewriting and not in its sibling.
        assert_eq!(
            blanks_for_outside_flags(WINDOWS, "rm -r --exclude=/ build").as_deref(),
            Some("rm -r --exclude=/ build")
        );
        // A value that begins a flag is read as one once the delimiter before
        // it becomes a blank — and read as a *flag*, so its own `=` is kept:
        // `x=-rf=z` is `x -rf=z`, which is what pins the token restart after a
        // replaced delimiter (without it, `-rf=z` is judged mid-token and its
        // `=` is blanked too).
        assert_eq!(
            blanks_for_outside_flags(WINDOWS, "x=-rf=z y=-rf").as_deref(),
            Some("x -rf=z y -rf")
        );
        // A flag token ends where the command does, not only at a blank: a
        // `-` word glued to a command separator must not carry the carve-out
        // into the next command, whose `=` is the one hiding the verb. Derived
        // from the grammar rather than copied out of it, so a separator added
        // to the field is covered the day it is added.
        for boundary in WINDOWS.command_separators {
            let glued = format!(r"echo --x=1{boundary}del=/f/s/q C:\*");
            let read = format!(r"echo --x=1{boundary}del /f/s/q C:\*");
            assert_eq!(
                blanks_for_outside_flags(WINDOWS, &glued).as_deref(),
                Some(read.as_str()),
                "{glued:?}: the flag must end at the command boundary"
            );
        }
        // A flag with no `=` of its own ends there too — the carve-out is
        // opened by the leading `-`, not by a delimiter the token contains.
        assert_eq!(
            blanks_for_outside_flags(WINDOWS, r"findstr -i|del=/f/s/q C:\*").as_deref(),
            Some(r"findstr -i|del /f/s/q C:\*")
        );
        // But not at `;`: it stays inside the token, so this token begins with
        // `d` and its `=` is read. Cutting there would make `-x=/s` a flag
        // and hand the spelling to the residual recorded for a delimiter
        // inside a flag's value; `blanks_for` and the closure take this
        // reading on to `del -x /s C:\*`.
        assert_eq!(
            blanks_for_outside_flags(WINDOWS, r"del;-x=/s;C:\*").as_deref(),
            Some(r"del;-x /s;C:\*")
        );
        // An empty set answers before allocating, which is what Unix pays.
        assert_eq!(blanks_for_outside_flags(SH, r"del=/f/s/q C:\*"), None);
    }

    /// A command boundary inside quotes is not one, and a quote at the head of
    /// a token opens no token of its own.
    ///
    /// Both halves invented a denial on a floor no mode can override. The `&`
    /// in a URL ended the flag token, so `curl --data="user=x&su=1" https://h`
    /// read as `…&su 1"`, which `segments` cut at the `&` into a segment whose
    /// program word is `su` — privilege escalation, for a line `cmd` runs with
    /// that `&` as a literal character. And a quote ahead of a flag hid the `-`
    /// behind it, so `"--exclude=/"` was no flag, its `=` was read, and
    /// `clean_token` handed the rule a bare `/`: `rm -r "--exclude=/" build`
    /// denied as a recursive remove of the root where the unquoted spelling
    /// passed.
    ///
    /// A delimiter *inside* the quotes is still read. `cmd` removes quotes
    /// before it splits words, so `del"="/f/s/q C:\*` really is the drive wipe,
    /// and that spelling is the one this rewriting exists for — reading the
    /// quotes as token boundaries rather than as quotes is what must stop, not
    /// reading the delimiter behind them.
    #[test]
    fn a_quoted_command_boundary_does_not_end_a_flag_token() {
        for (line, read) in [
            // The boundary inside the quotes is literal to the interpreter.
            (
                r#"curl --data="user=x&su=1" https://h"#,
                r#"curl --data="user=x&su=1" https://h"#,
            ),
            (
                r#"curl --url="https://x/?a=1|sh=2""#,
                r#"curl --url="https://x/?a=1|sh=2""#,
            ),
            // A quote ahead of the `-` leaves it a flag.
            (
                r#"rm -r "--exclude=/" build"#,
                r#"rm -r "--exclude=/" build"#,
            ),
            (r"rm -r '--exclude=/' build", r"rm -r '--exclude=/' build"),
            // But the delimiter behind a quote is still read, because the
            // interpreter strips the quote before it splits.
            (r#"del"="/f/s/q C:\*"#, r#"del" "/f/s/q C:\*"#),
            (r#""del=/f/s/q" C:\*"#, r#""del /f/s/q" C:\*"#),
            // And an unquoted boundary still ends the token.
            (r"echo --x=1&del=/f/s/q C:\*", r"echo --x=1&del /f/s/q C:\*"),
        ] {
            assert_eq!(
                blanks_for_outside_flags(WINDOWS, line).as_deref(),
                Some(read),
                "{line:?}"
            );
        }
    }

    /// Every character `segments` cuts a command at ends a flag token one way
    /// or another: as whitespace, as a word delimiter `blanks_for` blanks
    /// before the closure re-reads, or as one of the grammar's own
    /// `command_separators`. A separator added to `segments` that is none of
    /// these is a `|`-shaped hole reopened, and this names it.
    ///
    /// Asked of *both* grammars, which is the point of the field. Written as
    /// one module-wide constant, this asserted `cmd`'s answer for a set `sh`
    /// disagrees about (`;` ends a command there and delimits a word here), and
    /// `sh` passed only because its `=` set is empty and
    /// `blanks_for_outside_flags` returns before it ever asks. Run against `sh`,
    /// the constant failed.
    #[test]
    fn flag_tokens_end_at_every_command_boundary() {
        for grammar in [SH, WINDOWS] {
            for separator in SEGMENT_SEPARATORS {
                assert!(
                    separator.is_whitespace()
                        || grammar.word_delimiters.contains(&separator)
                        || grammar.command_separators.contains(&separator),
                    "{separator:?} cuts a command but ends no flag token"
                );
            }
            // And a command separator is a command boundary — never a character
            // a rewriting would have blanked anyway, which is what makes it a
            // set that needs its own rule.
            for boundary in grammar.command_separators {
                assert!(SEGMENT_SEPARATORS.contains(boundary));
                assert!(!grammar.word_delimiters.contains(boundary));
                assert!(!grammar.word_delimiters_outside_flags.contains(boundary));
            }
        }
    }

    /// The Windows *text* reading, which the deny floor uses: cmd.exe's escape
    /// character is stripped like Unix quoting, a `\` is kept because it is a
    /// path separator — and therefore splits a basename, which is why
    /// `r\m -rf /` is not the `rm` the floor denies on Unix. Asserted here
    /// from any host; the floor's end-to-end verdicts stay `#[cfg]`-split.
    #[test]
    fn the_windows_text_reading_is_pinned_from_any_host() {
        assert_eq!(clean_token_in(WINDOWS, "de^l"), "del");
        assert_eq!(clean_token_in(WINDOWS, r"r\m"), r"r\m");
        assert_eq!(clean_token_in(SH, r"r\m"), "rm");
        assert_eq!(basename_lower_in(WINDOWS, r"r\m"), "m");
        assert_eq!(basename_lower_in(SH, r"r\m"), "rm");
        assert_eq!(
            basename_lower_in(WINDOWS, r"C:\Windows\System32\REG.EXE"),
            "reg"
        );
    }

    #[test]
    fn unattended_parse_splits_words_like_the_shell() {
        assert_eq!(
            argvs("cargo test --all"),
            words(&[&["cargo", "test", "--all"]])
        );
        assert_eq!(
            argvs("cargo test --features \"a b\" 'c d'"),
            words(&[&["cargo", "test", "--features", "a b", "c d"]])
        );
        // Backslash spellings are platform-split; see
        // `backslash_reading_is_pinned_for_both_grammars`.
        // Quote removal glues adjacent quoted pieces into one word; a quoted
        // empty string is a real (empty) argument.
        assert_eq!(
            argvs("printf a\"b\"'c' '' \"\""),
            words(&[&["printf", "abc", "", ""]])
        );
        // Inside double quotes a backslash before anything but `"`/`\` stays.
        assert_eq!(
            argvs("printf '%s\\n' \"a\\tb\""),
            words(&[&["printf", "%s\\n", "a\\tb"]])
        );
        // `#` opens a comment only at the start of a word — and only where the
        // interpreter has comments at all, see
        // `hash_opens_a_comment_only_where_the_interpreter_has_comments`.
        assert_eq!(
            argvs_in(SH, "cargo build # not run"),
            Some(words(&[&["cargo", "build"]]))
        );
        assert_eq!(argvs("cargo build#x"), words(&[&["cargo", "build#x"]]));
        // Blank lines are nothing.
        assert_eq!(
            argvs("cargo build\n\n  --release\n"),
            words(&[&["cargo", "build"], &["--release"]])
        );
        // Tabs and other blanks separate words too; a trailing `;` is fine.
        assert_eq!(argvs("git\tstatus ;"), words(&[&["git", "status"]]));
    }

    #[test]
    fn unattended_parse_chains_with_the_two_sequencing_operators() {
        assert_eq!(
            argvs("cargo build && cargo test; echo done\ngit status"),
            words(&[
                &["cargo", "build"],
                &["cargo", "test"],
                &["echo", "done"],
                &["git", "status"],
            ])
        );
        assert_eq!(
            gates("cargo build && cargo test; echo done\ngit status"),
            vec![
                RunIf::Always,
                RunIf::PreviousSucceeded,
                RunIf::Always,
                RunIf::Always
            ]
        );
        // A newline after `&&` is allowed, and the gate survives it.
        assert_eq!(
            gates("cargo build &&\ncargo test"),
            vec![RunIf::Always, RunIf::PreviousSucceeded]
        );
    }

    /// Everything a shell would have to interpret beyond quoting and the two
    /// sequencing operators is refused, so it can neither be auto-approved nor
    /// run unattended.
    #[test]
    fn unattended_parse_refuses_what_only_a_shell_can_read() {
        for command in [
            "",
            "   ",
            "a | b",
            "a || b",
            "a &",
            "a && b &",
            "a &&",
            "&& a",
            "; a",
            "a ;; b",
            "a; ; b",
            "echo \"unterminated",
            "echo 'unterminated",
            "echo $HOME",
            "echo `id`",
            "echo *.rs",
            "echo a?",
            "echo [a]",
            "echo {a,b}",
            "echo (a)",
            "echo a > b",
            "echo a < b",
            "cd ~/x",
            "echo ~",
            "FOO=1 cargo test",
            "'' cargo test",
        ] {
            assert_eq!(parse_unattended(command), None, "{command:?}");
        }
        // A trailing backslash is an unfinished line to `sh` only: on Windows
        // it is the last character of a path. Platform-split alongside every
        // other backslash spelling, in
        // `backslash_reading_is_pinned_for_both_grammars`.
        #[cfg(unix)]
        assert_eq!(parse_unattended("echo done\\"), None);
    }

    /// The accepted grammar must mean to this parser exactly what it means to
    /// `sh`, or the words the gate judged are still not the words that run —
    /// the very gap this parser exists to close. Each spelling is run through
    /// the real shell as arguments to `printf '%s\n'`, one line per word, and
    /// compared with the parser's argv for the same text.
    #[cfg(unix)]
    #[test]
    fn unattended_parse_matches_sh_word_splitting() {
        for spelling in [
            "a b c",
            "\"a b\" c",
            "'c d' e",
            "e\\ f g",
            "\"q\\\"x\" y",
            "'it'\"'\"'s' z",
            "\"\" '' end",
            "a\"b\"c d",
            "x\\\\y z",
            "--flag=v --other=\"w x\"",
            "a\\tb c",
            "\"back\\\\slash\" \"quote\\\"d\" \"tab\\t\"",
            "tab\there",
            "line\\\ncontinued next",
            "trailing ;",
            "first # a comment",
            "hash#inside word",
            "'single \"double\" inside'",
            "\"double 'single' inside\"",
        ] {
            let command = format!("printf '%s\\n' {spelling}");
            let ours: Vec<String> = parse_unattended(&command)
                .unwrap_or_else(|| panic!("{command:?} must parse"))
                .into_iter()
                .flat_map(|cmd| cmd.argv.into_iter().skip(2))
                .collect();
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .stdin(std::process::Stdio::null())
                .output()
                .expect("run sh");
            assert!(output.status.success(), "{command:?} failed in sh");
            let theirs: Vec<&str> = std::str::from_utf8(&output.stdout)
                .expect("utf8")
                .lines()
                .collect();
            assert_eq!(ours, theirs, "{command:?} split differently from sh");
        }
    }

    /// A short option may carry its value glued on, and the fence has to read
    /// it: `cp -t/tmp ./x` is `cp -t /tmp ./x`, which every promise built on
    /// this predicate is about. The `=` spelling and the bare flags are in the
    /// same table so a future narrowing of one shows up as the other going
    /// red — the previous version passed every `=` case while returning
    /// `false` for the whole glued column.
    #[test]
    fn a_short_flags_glued_value_is_judged_like_the_operand_it_is() {
        let cases: &[(&str, bool)] = &[
            // Glued short-option values — the column that was unread.
            ("-t/tmp", true),
            ("-rft/tmp", true),
            ("-t~/x", true),
            ("-t../outside", true),
            (r"-tC:\x", true),
            (r"-t\\server\share", true),
            ("-t/tmp=x", true),
            // Glued, but in-tree: refused all the same, because where the
            // option letters end is the program's business. One prompt.
            ("-tbuild/out", true),
            // Bare flags and bundles carry no value at all.
            ("-r", false),
            ("-rf", false),
            ("--", false),
            ("-", false),
            ("--recursive", false),
            ("--exclude-from", false),
            ("-O2", false),
            ("-j8", false),
            // A long flag never glues: its value comes after `=` or in the
            // next word, so reading its name for one costs prompts for
            // nothing.
            ("--target-directory=/tmp", true),
            ("--target-directory=build/out", false),
            ("--exclude=~/.ssh", true),
            ("--exclude=/", true),
            ("--no-index", false),
            // Positional operands are unchanged.
            ("./x", false),
            ("/etc/passwd", true),
            ("~/.ssh/id_rsa", true),
            ("main..HEAD", false),
            ("..", true),
        ];
        for &(token, expected) in cases {
            assert_eq!(
                operand_leaves_cwd(token),
                expected,
                "{token:?} read the wrong way"
            );
        }
    }

    /// [`glued_value_leaves_cwd`] is the suffix scan written in one pass, and
    /// the two must stay the same predicate: the loop is what the fence means,
    /// the three conditions are only how it affords to run on every argument
    /// of every shell call. Enumerated over the characters that make the
    /// difference rather than over hand-picked spellings, because a term-level
    /// narrowing (dropping `~` from the `contains`, or `ends_with("..")`) is
    /// exactly what a table of examples keeps passing through.
    #[test]
    fn every_glued_value_reading_matches_the_suffix_scan() {
        fn by_suffix_scan(bundle: &str) -> bool {
            (0..bundle.len())
                .filter(|start| bundle.is_char_boundary(*start))
                .any(|start| escapes_cwd_by_spelling(&bundle[start..]))
        }
        let alphabet = ['t', '/', '\\', '~', '.', ':', 'C', '=', '1'];
        let mut checked = 0usize;
        for a in alphabet {
            for b in alphabet {
                for c in alphabet {
                    for d in alphabet {
                        let bundle: String = [a, b, c, d].iter().collect();
                        assert_eq!(
                            glued_value_leaves_cwd(&bundle),
                            by_suffix_scan(&bundle),
                            "{bundle:?} read differently by the two forms"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, alphabet.len().pow(4));
        // And the one-pass form stays linear: the loop it replaces is
        // quadratic, and a model can spell a flag token this long.
        let huge = format!("-t{}", "a".repeat(200_000));
        assert!(!operand_leaves_cwd(&huge));
    }

    fn sh_words(command: &str, bodies: HeredocBodies) -> Option<Vec<Vec<String>>> {
        sh_commands_in(SH, command, bodies).map(|read| read.commands)
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    /// What `sh` makes data stays data, and what it runs is read as a command:
    /// the reading the irreversible floor needs and the quote-blind
    /// [`segments`] could not give it.
    #[test]
    fn sh_commands_reads_quotes_continuations_and_comments_as_sh_does() {
        use HeredocBodies::Skip;
        assert_eq!(
            sh_words("a b; c 'd e' \"f g\" && h|i", Skip),
            Some(vec![
                argv(&["a", "b"]),
                argv(&["c", "d e", "f g"]),
                argv(&["h"]),
                argv(&["i"]),
            ])
        );
        // A `\`-newline joins; an escaped backslash before a newline does not.
        assert_eq!(
            sh_words("git push origin main \\\n  --force", Skip),
            Some(vec![argv(&["git", "push", "origin", "main", "--force"])])
        );
        assert_eq!(
            sh_words("echo a\\\\\nls", Skip),
            Some(vec![argv(&["echo", "a\\"]), argv(&["ls"])])
        );
        // A quoted newline or separator is part of the word.
        assert_eq!(
            sh_words("git commit -m \"ci: x\n\nnpm publish; now\"", Skip),
            Some(vec![argv(&[
                "git",
                "commit",
                "-m",
                "ci: x\n\nnpm publish; now"
            ])])
        );
        assert_eq!(
            sh_words("git commit -m 'a\nnpm publish'", Skip),
            Some(vec![argv(&["git", "commit", "-m", "a\nnpm publish"])])
        );
        // `#` opens a comment only at the start of a word.
        assert_eq!(
            sh_words("echo a#b # npm publish\nls", Skip),
            Some(vec![argv(&["echo", "a#b"]), argv(&["ls"])])
        );
        // A redirection's `&` is not a separator.
        assert_eq!(
            sh_words("npm test 2>&1 >&2", Skip),
            Some(vec![argv(&["npm", "test", "2>&1", ">&2"])])
        );
    }

    /// A substitution runs its commands wherever it sits, double quotes
    /// included, so each is a command of its own; a here-document body is the
    /// receiving command's stdin unless the caller asks for it to be read.
    #[test]
    fn sh_commands_reads_substitutions_and_steps_over_heredoc_bodies() {
        use HeredocBodies::{Read, Skip};
        assert_eq!(
            sh_words(
                "echo \"v=$(npm publish)\" `docker push x` (git push -f)",
                Skip
            ),
            Some(vec![
                argv(&["npm", "publish"]),
                argv(&["docker", "push", "x"]),
                argv(&["echo", "v=", ""]),
                argv(&["git", "push", "-f"]),
            ])
        );
        let commit =
            "git commit -m \"$(cat <<'EOF'\nfix\n\nnpm publish now runs in CI\nEOF\n)\" && git log";
        let read = sh_commands_in(SH, commit, Skip).expect("models this line");
        assert_eq!(
            read.commands,
            vec![
                argv(&["cat"]),
                argv(&["git", "commit", "-m", ""]),
                argv(&["git", "log"]),
            ]
        );
        assert!(read.skipped_heredoc);
        // `<<-` strips leading tabs before comparing the delimiter; a body
        // with no delimiter line runs to the end of input.
        assert_eq!(
            sh_words("git commit -F - <<-EOF\n\tnpm publish\n\tEOF\nls", Skip),
            Some(vec![argv(&["git", "commit", "-F", "-"]), argv(&["ls"])])
        );
        assert_eq!(
            sh_words("cat <<EOF\nnpm publish", Skip),
            Some(vec![argv(&["cat"])])
        );
        // Read: the body is shell lines.
        assert_eq!(
            sh_words("sh <<'EOF'\nnpm publish\nEOF", Read),
            Some(vec![
                argv(&["sh"]),
                argv(&["npm", "publish"]),
                argv(&["EOF"])
            ])
        );
        // A here-string is an ordinary operand, not a body.
        assert_eq!(
            sh_words("bash <<< 'npm publish'", Skip),
            Some(vec![argv(&["bash", "npm publish"])])
        );
    }

    /// How many commands had finished before anything was fed: earlier
    /// top-level pipelines only. A stage piped in (across a newline too), a
    /// loop around the body, a group or `if` that is piped into, and anything
    /// after a background `&` may still run once the body exists.
    #[test]
    fn sh_commands_counts_what_finished_before_a_heredoc() {
        let finished = |command: &str| {
            sh_commands_in(SH, command, HeredocBodies::Skip)
                .map(|read| read.finished_before_heredoc)
        };
        for (command, expected) in [
            ("cargo build && git commit -F - <<EOF\nmsg\nEOF", 1),
            ("make || git commit -F - <<EOF\nmsg\nEOF\nls", 1),
            ("a; b $(c) && git commit -F - <<EOF\nmsg\nEOF", 3),
            ("cat <<EOF | sh\nx\nEOF", 0),
            ("a; b | cat <<EOF\nx\nEOF", 1),
            // `|⏎` continues the pipeline: `a` runs alongside the body.
            ("a |\ncat > x <<EOF\nx\nEOF", 0),
            ("a &\ncat <<EOF\nx\nEOF", 0),
            // bash's `&>` redirects and backgrounds nothing (`> log` is a
            // command of its own as dash reads it, and finished too).
            ("a &> log && cat <<EOF\nx\nEOF", 2),
            ("{ a; cat <<EOF\nx\nEOF\n}", 1),
            ("if a; then cat <<EOF\nx\nEOF\nfi", 1),
            ("a | { b; cat <<EOF\nx\nEOF\n}", 0),
            ("a | if b; then cat <<EOF\nx\nEOF\nfi", 0),
            ("for i in 1 2; do a; cat > x <<EOF\nx\nEOF\ndone", 0),
            ("while a; do if b; then c; cat <<EOF\nx\nEOF\nfi; done", 0),
            // A reserved word right after `then`/`do` is one too.
            ("if a; then while b; do c; cat <<EOF\nx\nEOF\ndone; fi", 1),
            // A loop that closed first is finished.
            ("for i in 1 2; do a; done\ncat <<EOF\nx\nEOF", 3),
            // `done <<EOF` feeds the loop it closes.
            ("while read l; do a; done <<EOF\nx\nEOF", 0),
            // A quoted `done` is a command, not the loop's close.
            ("while a; do b; \"done\"; cat <<EOF\nx\nEOF\ndone", 0),
            // A here-string feeds like a body.
            ("a && bash <<< 'npm publish'", 1),
        ] {
            assert_eq!(finished(command), Some(expected), "{command:?}");
        }
        let read = sh_commands_in(SH, "a && bash <<< 'npm publish' x", HeredocBodies::Skip)
            .expect("models this line");
        assert_eq!(read.here_strings, vec!["npm publish".to_string()]);
        assert_eq!(
            read.commands,
            vec![argv(&["a"]), argv(&["bash", "npm publish", "x"])]
        );
    }

    /// `$((` is arithmetic when its `((` closes as `))` — even with groups
    /// inside — and otherwise, to bash, a substitution holding a subshell,
    /// whose commands run. A substitution inside arithmetic runs either way.
    #[test]
    fn sh_commands_reads_a_dollar_double_paren_as_bash_decides_it() {
        use HeredocBodies::Skip;
        for arithmetic in [
            "echo $(((1+2)*3))",
            "echo $(( (1+2) * 3 ))",
            "echo \"$((1+2))\"",
            "echo $((1<<2))",
        ] {
            assert_eq!(
                sh_words(arithmetic, Skip),
                Some(vec![argv(&["echo", ""])]),
                "{arithmetic:?}"
            );
        }
        // A group inside arithmetic is arithmetic too: its `<<` is a shift,
        // not a here-document that would swallow the next line.
        assert_eq!(
            sh_words("echo $(( (1<<2) ))\nnpm publish", Skip),
            Some(vec![argv(&["echo", ""]), argv(&["npm", "publish"])])
        );
        assert_eq!(
            sh_words("echo $(( $(npm publish) + 1 ))", Skip),
            Some(vec![argv(&["npm", "publish"]), argv(&["echo", ""])])
        );
        assert_eq!(
            sh_words("echo $((cd x && npm publish) | sh)", Skip),
            Some(vec![
                argv(&["cd", "x"]),
                argv(&["npm", "publish"]),
                argv(&["sh"]),
                argv(&["echo", ""]),
            ])
        );
        assert_eq!(
            sh_words("echo \"$((npm publish)\n)\"", Skip),
            Some(vec![argv(&["npm", "publish"]), argv(&["echo", ""])])
        );
    }

    /// What a `$((` holds is moved once, when it is decided, and a command's
    /// reserved words are not re-read per word. Handing held commands outward
    /// at each group's `)` moved each one once per group around it (14.6 s for
    /// the nest below), and asking "only reserved words so far?" of the whole
    /// argv at every word re-read it per word (59.6 s for the `!`s). Healthy,
    /// each is tens of milliseconds — orders of magnitude apart, which is when
    /// a clock can tell them apart (see `shell_deny`'s `cost` module).
    #[test]
    fn sh_commands_reads_nests_and_reserved_words_in_one_pass() {
        use HeredocBodies::Skip;
        let depth = 100_000;
        let nest = format!(
            "echo $(({}{}{}))\nnpm publish",
            "(".repeat(depth),
            "a;".repeat(depth),
            ")".repeat(depth)
        );
        let start = std::time::Instant::now();
        assert_eq!(
            sh_words(&nest, Skip),
            Some(vec![argv(&["echo", ""]), argv(&["npm", "publish"])])
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "held commands moved once per group: {:?}",
            start.elapsed()
        );
        let bangs = format!("{}npm publish", "! ".repeat(depth));
        let start = std::time::Instant::now();
        assert_eq!(
            sh_words(&bangs, Skip).map(|commands| commands.len()),
            Some(1)
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "reserved words re-read per word: {:?}",
            start.elapsed()
        );
    }

    /// Contexts whose contents are not commands, or whose `)` is not a closer:
    /// arithmetic (its `<<` is a shift — read as a here-document it swallowed
    /// every line after it), an array, and a `case` pattern inside `$( … )`.
    /// And a quoted `<` is text, so the `&` after it is a separator, while the
    /// `|` of `>|` belongs to its redirection.
    #[test]
    fn sh_commands_reads_arithmetic_arrays_and_case_patterns_as_sh_does() {
        use HeredocBodies::Skip;
        assert_eq!(
            sh_words("echo $((1<<2))\nnpm publish", Skip),
            Some(vec![argv(&["echo", ""]), argv(&["npm", "publish"])])
        );
        // `((` at the head of a command is arithmetic to bash and two
        // subshells to dash, which RUNS what is inside — so it is read as a
        // command; its `<<` is still no here-document.
        assert_eq!(
            sh_words("(( x = 1 << 4 ))\nls", Skip),
            Some(vec![argv(&["x", "=", "1", "<<", "4"]), argv(&["ls"])])
        );
        assert_eq!(
            sh_words("args=(git push --force) && ls", Skip),
            Some(vec![argv(&["args="]), argv(&["ls"])])
        );
        assert_eq!(
            sh_words("echo \"$(case $x in a) npm publish;; esac)\"", Skip),
            Some(vec![
                argv(&["case", "$x", "in", "a"]),
                argv(&["npm", "publish"]),
                argv(&["esac"]),
                argv(&["echo", ""]),
            ])
        );
        assert_eq!(
            sh_words("echo \"<\"& npm publish", Skip),
            Some(vec![argv(&["echo", "<"]), argv(&["npm", "publish"])])
        );
        assert_eq!(
            sh_words("make 2>&1 >&2", Skip),
            Some(vec![argv(&["make", "2>&1", ">&2"])])
        );
        // `>|` is a redirection that overrides noclobber, not a pipe.
        assert_eq!(
            sh_words("cat >| out.txt <<'EOF'\nx\nEOF", Skip),
            Some(vec![argv(&["cat", ">|", "out.txt"])])
        );
    }

    /// What it does not model is `None`, for the caller to read blind instead
    /// of guessing — including every grammar but `sh`'s.
    #[test]
    fn sh_commands_refuses_what_it_does_not_model() {
        for command in [
            "npm publish \"",
            "npm publish 'x",
            "echo $(npm publish",
            "echo `npm publish",
            "echo \\",
            "echo $'a\\'b'",
            "echo $\"x\"",
            "echo `a \\`npm publish\\` b`",
            "cat <<\nx",
        ] {
            assert_eq!(
                sh_commands_in(SH, command, HeredocBodies::Skip),
                None,
                "{command:?}"
            );
        }
        assert_eq!(
            sh_commands_in(WINDOWS, "npm publish", HeredocBodies::Skip),
            None,
            "cmd quotes, escapes and expands differently"
        );
    }
}
