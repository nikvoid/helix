use std::fmt::Write;
use std::io::BufReader;
use std::ops::Deref;

use crate::job::Job;

use super::*;

use helix_core::fuzzy::fuzzy_match;
use helix_core::indent::MAX_INDENT;
use helix_core::{line_ending, shellwords::Shellwords};
use helix_stdx::path::home_dir;
use helix_view::document::{read_to_string, DEFAULT_LANGUAGE_NAME};
use helix_view::editor::{CloseError, ConfigEvent};
use serde_json::Value;
use shellwords::Args;
use ui::completers::{self, Completer};

#[derive(Clone)]
pub struct TypableCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub doc: &'static str,
    // params, flags, helper, completer
    pub fun: fn(&mut compositor::Context, Args, PromptEvent) -> anyhow::Result<()>,
    /// What completion methods, if any, does this command have?
    pub signature: CommandSignature,
}

impl TypableCommand {
    fn completer_for_argument_number(&self, n: usize) -> &Completer {
        self.signature
            .positional_args
            .get(n)
            .map_or(&self.signature.var_args, |completer| completer)
    }
}

#[derive(Clone)]
pub struct CommandSignature {
    // Arguments with specific completion methods based on their position.
    positional_args: &'static [Completer],

    // All remaining arguments will use this completion method, if set.
    var_args: Completer,
}

impl CommandSignature {
    const fn none() -> Self {
        Self {
            positional_args: &[],
            var_args: completers::none,
        }
    }

    const fn positional(completers: &'static [Completer]) -> Self {
        Self {
            positional_args: completers,
            var_args: completers::none,
        }
    }

    const fn all(completer: Completer) -> Self {
        Self {
            positional_args: &[],
            var_args: completer,
        }
    }
}

fn quit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    log::debug!("quitting...");

    if event != PromptEvent::Validate {
        return Ok(());
    }

    ensure!(args.is_empty(), ":quit takes no arguments");

    // last view and we have unsaved changes
    if cx.editor.tree.views().count() == 1 {
        buffers_remaining_impl(cx.editor)?;
    }

    cx.block_try_flush_writes()?;
    cx.editor.close(view!(cx.editor).id);

    Ok(())
}

fn force_quit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    ensure!(args.is_empty(), ":quit! takes no arguments");

    cx.block_try_flush_writes()?;
    cx.editor.close(view!(cx.editor).id);

    Ok(())
}

fn open(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    ensure!(!args.is_empty(), ":open needs at least one argument");

    for arg in args {
        let (path, pos) = args::parse_file(arg);
        let path = helix_stdx::path::expand_tilde(path);
        // If the path is a directory, open a file picker on that directory and update the status
        // message
        if let Ok(true) = std::fs::canonicalize(&path).map(|p| p.is_dir()) {
            let callback = async move {
                let call: job::Callback = job::Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor: &mut Compositor| {
                        let picker = ui::file_picker(path.into_owned(), &editor.config());
                        compositor.push(Box::new(overlaid(picker)));
                    },
                ));
                Ok(call)
            };
            cx.jobs.callback(callback);
        } else {
            // Otherwise, just open the file
            let _ = cx.editor.open(&path, Action::Replace)?;
            let (view, doc) = current!(cx.editor);
            let pos = Selection::point(pos_at_coords(doc.text().slice(..), pos, true));
            doc.set_selection(view.id, pos);
            // does not affect opening a buffer without pos
            align_view(doc, view, Align::Center);
        }
    }
    Ok(())
}

fn buffer_close_by_ids_impl(
    cx: &mut compositor::Context,
    doc_ids: &[DocumentId],
    force: bool,
) -> anyhow::Result<()> {
    cx.block_try_flush_writes()?;

    let (modified_ids, modified_names): (Vec<_>, Vec<_>) = doc_ids
        .iter()
        .filter_map(|&doc_id| {
            if let Err(CloseError::BufferModified(name)) = cx.editor.close_document(doc_id, force) {
                Some((doc_id, name))
            } else {
                None
            }
        })
        .unzip();

    if let Some(first) = modified_ids.first() {
        let current = doc!(cx.editor);
        // If the current document is unmodified, and there are modified
        // documents, switch focus to the first modified doc.
        if !modified_ids.contains(&current.id()) {
            cx.editor.switch(*first, Action::Replace);
        }
        bail!(
            "{} unsaved buffer{} remaining: {:?}",
            modified_names.len(),
            if modified_names.len() == 1 { "" } else { "s" },
            modified_names,
        );
    }

    Ok(())
}

fn buffer_gather_paths_impl(editor: &mut Editor, args: Args) -> Vec<DocumentId> {
    // No arguments implies current document
    if args.is_empty() {
        let doc_id = view!(editor).doc;
        return vec![doc_id];
    }

    let mut nonexistent_buffers = vec![];
    let mut document_ids = vec![];
    for arg in args {
        let doc_id = editor.documents().find_map(|doc| {
            let arg_path = Some(Path::new(arg));
            if doc.path().map(|p| p.as_path()) == arg_path
                || doc.relative_path().as_deref() == arg_path
            {
                Some(doc.id())
            } else {
                None
            }
        });

        match doc_id {
            Some(doc_id) => document_ids.push(doc_id),
            None => nonexistent_buffers.push(format!("'{}'", arg)),
        }
    }

    if !nonexistent_buffers.is_empty() {
        editor.set_error(format!(
            "cannot close non-existent buffers: {}",
            nonexistent_buffers.join(", ")
        ));
    }

    document_ids
}

fn buffer_close(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_buffer_close(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, true)
}

fn buffer_gather_others_impl(editor: &mut Editor) -> Vec<DocumentId> {
    let current_document = &doc!(editor).id();
    editor
        .documents()
        .map(|doc| doc.id())
        .filter(|doc_id| doc_id != current_document)
        .collect()
}

fn buffer_close_others(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_others_impl(cx.editor);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_buffer_close_others(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_others_impl(cx.editor);
    buffer_close_by_ids_impl(cx, &document_ids, true)
}

fn buffer_gather_all_impl(editor: &mut Editor) -> Vec<DocumentId> {
    editor.documents().map(|doc| doc.id()).collect()
}

fn buffer_close_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_all_impl(cx.editor);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_buffer_close_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let document_ids = buffer_gather_all_impl(cx.editor);
    buffer_close_by_ids_impl(cx, &document_ids, true)
}

fn buffer_next(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    goto_buffer(cx.editor, Direction::Forward, 1);
    Ok(())
}

fn buffer_previous(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    goto_buffer(cx.editor, Direction::Backward, 1);
    Ok(())
}

fn write_impl(cx: &mut compositor::Context, path: Option<&str>, force: bool) -> anyhow::Result<()> {
    let config = cx.editor.config();
    let jobs = &mut cx.jobs;
    let (view, doc) = current!(cx.editor);

    if config.insert_final_newline {
        insert_final_newline(doc, view.id);
    }

    // Save an undo checkpoint for any outstanding changes.
    doc.append_changes_to_history(view);

    let fmt = if config.auto_format {
        doc.auto_format().map(|fmt| {
            let callback = make_format_callback(
                doc.id(),
                doc.version(),
                view.id,
                fmt,
                Some((path.map(Into::into), force)),
            );

            jobs.add(Job::with_callback(callback).wait_before_exiting());
        })
    } else {
        None
    };

    if fmt.is_none() {
        let id = doc.id();
        cx.editor.save(id, path, force)?;
    }

    Ok(())
}

fn insert_final_newline(doc: &mut Document, view_id: ViewId) {
    let text = doc.text();
    if line_ending::get_line_ending(&text.slice(..)).is_none() {
        let eof = Selection::point(text.len_chars());
        let insert = Transaction::insert(text, &eof, doc.line_ending.as_str().into());
        doc.apply(&insert, view_id);
    }
}

fn write(cx: &mut compositor::Context, mut args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(cx, args.next(), false)
}

fn force_write(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(cx, args.next(), true)
}

fn write_buffer_close(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(cx, args.next(), false)?;

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn force_write_buffer_close(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(cx, args.next(), true)?;

    let document_ids = buffer_gather_paths_impl(cx.editor, args);
    buffer_close_by_ids_impl(cx, &document_ids, false)
}

fn new_file(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.new_file(Action::Replace);

    Ok(())
}

fn format(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current!(cx.editor);
    let format = doc.format().context(
        "A formatter isn't available, and no language server provides formatting capabilities",
    )?;
    let callback = make_format_callback(doc.id(), doc.version(), view.id, format, None);
    cx.jobs.callback(callback);

    Ok(())
}

fn set_indent_style(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    use IndentStyle::*;

    // If no argument, report current indent style.
    if args.is_empty() {
        let style = doc!(cx.editor).indent_style;
        cx.editor.set_status(match style {
            Tabs => "tabs".to_owned(),
            Spaces(1) => "1 space".to_owned(),
            Spaces(n) => format!("{} spaces", n),
        });
        return Ok(());
    }

    // Attempt to parse argument as an indent style.
    let style = match args.next() {
        Some(arg) if "tabs".starts_with(&arg.to_lowercase()) => Some(Tabs),
        Some("0") => Some(Tabs),
        Some(arg) => arg
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=MAX_INDENT).contains(n))
            .map(Spaces),
        _ => None,
    };

    let style = style.context("invalid indent style")?;
    let doc = doc_mut!(cx.editor);
    doc.indent_style = style;

    Ok(())
}

/// Sets or reports the current document's line ending setting.
fn set_line_ending(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    use LineEnding::*;

    // If no argument, report current line ending setting.
    if args.is_empty() {
        let line_ending = doc!(cx.editor).line_ending;
        cx.editor.set_status(match line_ending {
            Crlf => "crlf",
            LF => "line feed",
            #[cfg(feature = "unicode-lines")]
            FF => "form feed",
            #[cfg(feature = "unicode-lines")]
            CR => "carriage return",
            #[cfg(feature = "unicode-lines")]
            Nel => "next line",

            // These should never be a document's default line ending.
            #[cfg(feature = "unicode-lines")]
            VT | LS | PS => "error",
        });

        return Ok(());
    }

    let arg = args
        .next()
        .context("argument missing")?
        .to_ascii_lowercase();

    // Attempt to parse argument as a line ending.
    let line_ending = match arg {
        arg if arg.starts_with("crlf") => Crlf,
        arg if arg.starts_with("lf") => LF,
        #[cfg(feature = "unicode-lines")]
        arg if arg.starts_with("cr") => CR,
        #[cfg(feature = "unicode-lines")]
        arg if arg.starts_with("ff") => FF,
        #[cfg(feature = "unicode-lines")]
        arg if arg.starts_with("nel") => Nel,
        _ => bail!("invalid line ending"),
    };
    let (view, doc) = current!(cx.editor);
    doc.line_ending = line_ending;

    let mut pos = 0;
    let transaction = Transaction::change(
        doc.text(),
        doc.text().lines().filter_map(|line| {
            pos += line.len_chars();
            match helix_core::line_ending::get_line_ending(&line) {
                Some(ending) if ending != line_ending => {
                    let start = pos - ending.len_chars();
                    let end = pos;
                    Some((start, end, Some(line_ending.as_str().into())))
                }
                _ => None,
            }
        }),
    );
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);

    Ok(())
}
fn earlier(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let uk = args.raw().parse::<UndoKind>().map_err(|s| anyhow!(s))?;

    let (view, doc) = current!(cx.editor);
    let success = doc.earlier(view, uk);
    if !success {
        cx.editor.set_status("Already at oldest change");
    }

    Ok(())
}

fn later(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let uk = args.raw().parse::<UndoKind>().map_err(|s| anyhow!(s))?;

    let (view, doc) = current!(cx.editor);
    let success = doc.later(view, uk);
    if !success {
        cx.editor.set_status("Already at newest change");
    }

    Ok(())
}

fn write_quit(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(cx, args.next(), false)?;
    cx.block_try_flush_writes()?;
    quit(cx, Args::empty(), event)
}

fn force_write_quit(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_impl(cx, args.next(), true)?;
    cx.block_try_flush_writes()?;
    force_quit(cx, Args::empty(), event)
}

/// Results in an error if there are modified buffers remaining and sets editor
/// error, otherwise returns `Ok(())`. If the current document is unmodified,
/// and there are modified documents, switches focus to one of them.
pub(super) fn buffers_remaining_impl(editor: &mut Editor) -> anyhow::Result<()> {
    let (modified_ids, modified_names): (Vec<_>, Vec<_>) = editor
        .documents()
        .filter(|doc| doc.is_modified())
        .map(|doc| (doc.id(), doc.display_name()))
        .unzip();
    if let Some(first) = modified_ids.first() {
        let current = doc!(editor);
        // If the current document is unmodified, and there are modified
        // documents, switch focus to the first modified doc.
        if !modified_ids.contains(&current.id()) {
            editor.switch(*first, Action::Replace);
        }
        bail!(
            "{} unsaved buffer{} remaining: {:?}",
            modified_names.len(),
            if modified_names.len() == 1 { "" } else { "s" },
            modified_names,
        );
    }
    Ok(())
}

pub fn write_all_impl(
    cx: &mut compositor::Context,
    force: bool,
    write_scratch: bool,
) -> anyhow::Result<()> {
    let mut errors: Vec<&'static str> = Vec::new();
    let config = cx.editor.config();
    let jobs = &mut cx.jobs;
    let saves: Vec<_> = cx
        .editor
        .documents
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|id| {
            let doc = doc!(cx.editor, &id);
            if !doc.is_modified() {
                return None;
            }
            if doc.path().is_none() {
                if write_scratch {
                    errors.push("cannot write a buffer without a filename");
                }
                return None;
            }

            // Look for a view to apply the formatting change to.
            let target_view = cx.editor.get_synced_view_id(doc.id());
            Some((id, target_view))
        })
        .collect();

    for (doc_id, target_view) in saves {
        let doc = doc_mut!(cx.editor, &doc_id);
        let view = view_mut!(cx.editor, target_view);

        if config.insert_final_newline {
            insert_final_newline(doc, target_view);
        }

        // Save an undo checkpoint for any outstanding changes.
        doc.append_changes_to_history(view);

        let fmt = if config.auto_format {
            doc.auto_format().map(|fmt| {
                let callback = make_format_callback(
                    doc_id,
                    doc.version(),
                    target_view,
                    fmt,
                    Some((None, force)),
                );
                jobs.add(Job::with_callback(callback).wait_before_exiting());
            })
        } else {
            None
        };

        if fmt.is_none() {
            cx.editor.save::<PathBuf>(doc_id, None, force)?;
        }
    }

    if !errors.is_empty() && !force {
        bail!("{:?}", errors);
    }

    Ok(())
}

fn write_all(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_all_impl(cx, false, true)
}

fn force_write_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    write_all_impl(cx, true, true)
}

fn write_all_quit(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    write_all_impl(cx, false, true)?;
    quit_all_impl(cx, false)
}

fn force_write_all_quit(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let _ = write_all_impl(cx, true, true);
    quit_all_impl(cx, true)
}

fn quit_all_impl(cx: &mut compositor::Context, force: bool) -> anyhow::Result<()> {
    cx.block_try_flush_writes()?;
    if !force {
        buffers_remaining_impl(cx.editor)?;
    }

    // close all views
    let views: Vec<_> = cx.editor.tree.views().map(|(view, _)| view.id).collect();
    for view_id in views {
        cx.editor.close(view_id);
    }

    Ok(())
}

fn quit_all(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    quit_all_impl(cx, false)
}

fn force_quit_all(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    quit_all_impl(cx, true)
}

fn cquit(cx: &mut compositor::Context, mut args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let exit_code = args
        .next()
        .and_then(|code| code.parse::<i32>().ok())
        .unwrap_or(1);

    cx.editor.exit_code = exit_code;
    quit_all_impl(cx, false)
}

fn force_cquit(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let exit_code = args
        .next()
        .and_then(|code| code.parse::<i32>().ok())
        .unwrap_or(1);
    cx.editor.exit_code = exit_code;

    quit_all_impl(cx, true)
}

fn theme(cx: &mut compositor::Context, mut args: Args, event: PromptEvent) -> anyhow::Result<()> {
    let true_color = cx.editor.config.load().true_color || crate::true_color();
    match event {
        PromptEvent::Abort => {
            cx.editor.unset_theme_preview();
        }
        PromptEvent::Update => {
            if args.is_empty() {
                // Ensures that a preview theme gets cleaned up if the user backspaces until the prompt is empty.
                cx.editor.unset_theme_preview();
            } else if let Some(theme_name) = args.next() {
                if let Ok(theme) = cx.editor.theme_loader.load(theme_name) {
                    if !(true_color || theme.is_16_color()) {
                        bail!("Unsupported theme: theme requires true color support");
                    }
                    cx.editor.set_theme_preview(theme);
                };
            };
        }
        PromptEvent::Validate => {
            if let Some(theme_name) = args.next() {
                let theme = cx
                    .editor
                    .theme_loader
                    .load(theme_name)
                    .map_err(|err| anyhow::anyhow!("Could not load theme: {}", err))?;
                if !(true_color || theme.is_16_color()) {
                    bail!("Unsupported theme: theme requires true color support");
                }
                cx.editor.set_theme(theme);
            } else {
                let name = cx.editor.theme.name().to_string();

                cx.editor.set_status(name);
            }
        }
    };

    Ok(())
}

fn yank_main_selection_to_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    yank_primary_selection_impl(cx.editor, '+');
    Ok(())
}

fn yank_joined(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let register = cx.editor.selected_register.unwrap_or('"');
    yank_joined_impl(cx.editor, args.raw(), register);
    Ok(())
}

fn yank_joined_to_clipboard(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    yank_joined_impl(cx.editor, args.raw(), '+');
    Ok(())
}

fn yank_main_selection_to_primary_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    yank_primary_selection_impl(cx.editor, '*');
    Ok(())
}

fn yank_joined_to_primary_clipboard(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    yank_joined_impl(cx.editor, args.raw(), '*');
    Ok(())
}

fn paste_clipboard_after(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    paste(cx.editor, '+', Paste::After, 1);
    Ok(())
}

fn paste_clipboard_before(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    paste(cx.editor, '+', Paste::Before, 1);
    Ok(())
}

fn paste_primary_clipboard_after(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    paste(cx.editor, '*', Paste::After, 1);
    Ok(())
}

fn paste_primary_clipboard_before(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    paste(cx.editor, '*', Paste::Before, 1);
    Ok(())
}

fn replace_selections_with_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    replace_with_yanked_impl(cx.editor, '+', 1);
    Ok(())
}

fn replace_selections_with_primary_clipboard(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    replace_with_yanked_impl(cx.editor, '*', 1);
    Ok(())
}

fn show_clipboard_provider(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    cx.editor
        .set_status(cx.editor.registers.clipboard_provider_name());
    Ok(())
}

fn change_current_directory(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let dir = match args.next() {
        Some("-") => cx
            .editor
            .last_cwd
            .clone()
            .ok_or_else(|| anyhow!("No previous working directory"))?,
        Some(path) => helix_stdx::path::expand_tilde(Path::new(path)).to_path_buf(),
        None => home_dir()?,
    };

    cx.editor.last_cwd = helix_stdx::env::set_current_working_dir(dir)?;

    cx.editor.set_status(format!(
        "Current working directory is now {}",
        helix_stdx::env::current_working_dir().display()
    ));

    Ok(())
}

fn show_current_directory(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let cwd = helix_stdx::env::current_working_dir();
    let message = format!("Current working directory is {}", cwd.display());

    if cwd.exists() {
        cx.editor.set_status(message);
    } else {
        cx.editor.set_error(format!("{message} (deleted)"));
    }
    Ok(())
}

/// Sets the [`Document`]'s encoding..
fn set_encoding(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc_mut!(cx.editor);
    if let Some(label) = args.next() {
        doc.set_encoding(label)
    } else {
        let encoding = doc.encoding().name().to_owned();
        cx.editor.set_status(encoding);
        Ok(())
    }
}

/// Shows info about the character under the primary cursor.
fn get_character_info(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().slice(..);

    let grapheme_start = doc.selection(view.id).primary().cursor(text);
    let grapheme_end = graphemes::next_grapheme_boundary(text, grapheme_start);

    if grapheme_start == grapheme_end {
        return Ok(());
    }

    let grapheme = text.slice(grapheme_start..grapheme_end).to_string();
    let encoding = doc.encoding();

    let printable = grapheme.chars().fold(String::new(), |mut s, c| {
        match c {
            '\0' => s.push_str("\\0"),
            '\t' => s.push_str("\\t"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            _ => s.push(c),
        }

        s
    });

    // Convert to Unicode codepoints if in UTF-8
    let unicode = if encoding == encoding::UTF_8 {
        let mut unicode = " (".to_owned();

        for (i, char) in grapheme.chars().enumerate() {
            if i != 0 {
                unicode.push(' ');
            }

            unicode.push_str("U+");

            let codepoint: u32 = if char.is_ascii() {
                char.into()
            } else {
                // Not ascii means it will be multi-byte, so strip out the extra
                // bits that encode the length & mark continuation bytes

                let s = String::from(char);
                let bytes = s.as_bytes();

                // First byte starts with 2-4 ones then a zero, so strip those off
                let first = bytes[0];
                let codepoint = first & (0xFF >> (first.leading_ones() + 1));
                let mut codepoint = u32::from(codepoint);

                // Following bytes start with 10
                for byte in bytes.iter().skip(1) {
                    codepoint <<= 6;
                    codepoint += u32::from(*byte) & 0x3F;
                }

                codepoint
            };

            write!(unicode, "{codepoint:0>4x}").unwrap();
        }

        unicode.push(')');
        unicode
    } else {
        String::new()
    };

    // Give the decimal value for ascii characters
    let dec = if encoding.is_ascii_compatible() && grapheme.len() == 1 {
        format!(" Dec {}", grapheme.as_bytes()[0])
    } else {
        String::new()
    };

    let hex = {
        let mut encoder = encoding.new_encoder();
        let max_encoded_len = encoder
            .max_buffer_length_from_utf8_without_replacement(grapheme.len())
            .unwrap();
        let mut bytes = Vec::with_capacity(max_encoded_len);
        let mut current_byte = 0;
        let mut hex = String::new();

        for (i, char) in grapheme.chars().enumerate() {
            if i != 0 {
                hex.push_str(" +");
            }

            let (result, _input_bytes_read) = encoder.encode_from_utf8_to_vec_without_replacement(
                &char.to_string(),
                &mut bytes,
                true,
            );

            if let encoding::EncoderResult::Unmappable(char) = result {
                bail!("{char:?} cannot be mapped to {}", encoding.name());
            }

            for byte in &bytes[current_byte..] {
                write!(hex, " {byte:0>2x}").unwrap();
            }

            current_byte = bytes.len();
        }

        hex
    };

    cx.editor
        .set_status(format!("\"{printable}\"{unicode}{dec} Hex{hex}"));

    Ok(())
}

/// Reload the [`Document`] from its source file.
fn reload(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    doc.reload(view, &cx.editor.diff_providers).map(|_| {
        view.ensure_cursor_in_view(doc, scrolloff);
    })?;
    if let Some(path) = doc.path() {
        cx.editor
            .language_servers
            .file_event_handler
            .file_changed(path.clone());
    }
    Ok(())
}

fn reload_all(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let view_id = view!(cx.editor).id;

    let docs_view_ids: Vec<(DocumentId, Vec<ViewId>)> = cx
        .editor
        .documents_mut()
        .map(|doc| {
            let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();

            if view_ids.is_empty() {
                doc.ensure_view_init(view_id);
                view_ids.push(view_id);
            };

            (doc.id(), view_ids)
        })
        .collect();

    for (doc_id, view_ids) in docs_view_ids {
        let doc = doc_mut!(cx.editor, &doc_id);

        // Every doc is guaranteed to have at least 1 view at this point.
        let view = view_mut!(cx.editor, view_ids[0]);

        // Ensure that the view is synced with the document's history.
        view.sync_changes(doc);

        if let Err(error) = doc.reload(view, &cx.editor.diff_providers) {
            cx.editor.set_error(format!("{}", error));
            continue;
        }

        if let Some(path) = doc.path() {
            cx.editor
                .language_servers
                .file_event_handler
                .file_changed(path.clone());
        }

        for view_id in view_ids {
            let view = view_mut!(cx.editor, view_id);
            if view.doc.eq(&doc_id) {
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }

    Ok(())
}

/// Update the [`Document`] if it has been modified.
fn update(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (_view, doc) = current!(cx.editor);
    if doc.is_modified() {
        write(cx, args, event)
    } else {
        Ok(())
    }
}

fn lsp_workspace_command(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let doc = doc!(cx.editor);
    let ls_id_commands = doc
        .language_servers_with_feature(LanguageServerFeature::WorkspaceCommand)
        .flat_map(|ls| {
            ls.capabilities()
                .execute_command_provider
                .iter()
                .flat_map(|options| options.commands.iter())
                .map(|command| (ls.id(), command))
        });

    if args.is_empty() {
        let commands = ls_id_commands
            .map(|(ls_id, command)| {
                (
                    ls_id,
                    helix_lsp::lsp::Command {
                        title: command.clone(),
                        command: command.clone(),
                        arguments: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        let callback = async move {
            let call: job::Callback = Callback::EditorCompositor(Box::new(
                move |_editor: &mut Editor, compositor: &mut Compositor| {
                    let columns = [ui::PickerColumn::new(
                        "title",
                        |(_ls_id, command): &(_, helix_lsp::lsp::Command), _| {
                            command.title.as_str().into()
                        },
                    )];
                    let picker = ui::Picker::new(
                        columns,
                        0,
                        commands,
                        (),
                        move |cx, (ls_id, command), _action| {
                            execute_lsp_command(cx.editor, *ls_id, command.clone());
                        },
                    );
                    compositor.push(Box::new(overlaid(picker)))
                },
            ));
            Ok(call)
        };
        cx.jobs.callback(callback);
    } else {
        let command = args.raw().to_string();

        let matches: Vec<_> = ls_id_commands
            .filter(|(_ls_id, c)| *c == &command)
            .collect();

        match matches.as_slice() {
            [(ls_id, _command)] => {
                execute_lsp_command(
                    cx.editor,
                    *ls_id,
                    helix_lsp::lsp::Command {
                        title: command.clone(),
                        arguments: None,
                        command,
                    },
                );
            }
            [] => {
                cx.editor.set_status(format!(
                    "`{command}` is not supported for any language server"
                ));
            }
            _ => {
                cx.editor.set_status(format!(
                    "`{command}` supported by multiple language servers"
                ));
            }
        }
    }
    Ok(())
}

fn lsp_restart(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let editor_config = cx.editor.config.load();
    let (_view, doc) = current!(cx.editor);
    let config = doc
        .language_config()
        .context("LSP not defined for the current document")?;

    cx.editor.language_servers.restart(
        config,
        doc.path(),
        &editor_config.workspace_lsp_roots,
        editor_config.lsp.snippets,
    )?;

    // This collect is needed because refresh_language_server would need to re-borrow editor.
    let document_ids_to_refresh: Vec<DocumentId> = cx
        .editor
        .documents()
        .filter_map(|doc| match doc.language_config() {
            Some(config)
                if config.language_servers.iter().any(|ls| {
                    config
                        .language_servers
                        .iter()
                        .any(|restarted_ls| restarted_ls.name == ls.name)
                }) =>
            {
                Some(doc.id())
            }
            _ => None,
        })
        .collect();

    for document_id in document_ids_to_refresh {
        cx.editor.refresh_language_servers(document_id);
    }

    Ok(())
}

fn lsp_stop(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let ls_shutdown_names = doc!(cx.editor)
        .language_servers()
        .map(|ls| ls.name().to_string())
        .collect::<Vec<_>>();

    for ls_name in &ls_shutdown_names {
        cx.editor.language_servers.stop(ls_name);

        for doc in cx.editor.documents_mut() {
            if let Some(client) = doc.remove_language_server_by_name(ls_name) {
                doc.clear_diagnostics(Some(client.id()));
                doc.reset_all_inlay_hints();
                doc.inlay_hints_oudated = true;
            }
        }
    }

    Ok(())
}

fn tree_sitter_scopes(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let pos = doc.selection(view.id).primary().cursor(text);
    let scopes = indent::get_scopes(doc.syntax(), text, pos);

    let contents = format!("```json\n{:?}\n````", scopes);

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let contents = ui::Markdown::new(contents, editor.syn_loader.clone());
                let popup = Popup::new("hover", contents).auto_close(true);
                compositor.replace_or_push("hover", popup);
            },
        ));
        Ok(call)
    };

    cx.jobs.callback(callback);

    Ok(())
}

fn tree_sitter_highlight_name(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    fn find_highlight_at_cursor(
        cx: &mut compositor::Context<'_>,
    ) -> Option<helix_core::syntax::Highlight> {
        use helix_core::syntax::HighlightEvent;

        let (view, doc) = current!(cx.editor);
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let byte = text.char_to_byte(cursor);
        let node = syntax.descendant_for_byte_range(byte, byte)?;
        // Query the same range as the one used in syntax highlighting.
        let range = {
            // Calculate viewport byte ranges:
            let row = text.char_to_line(doc.view_offset(view.id).anchor.min(text.len_chars()));
            // Saturating subs to make it inclusive zero indexing.
            let last_line = text.len_lines().saturating_sub(1);
            let height = view.inner_area(doc).height;
            let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
            let start = text.line_to_byte(row.min(last_line));
            let end = text.line_to_byte(last_visible_line + 1);

            start..end
        };

        let mut highlight = None;

        for event in syntax.highlight_iter(text, Some(range), None) {
            match event.unwrap() {
                HighlightEvent::Source { start, end }
                    if start == node.start_byte() && end == node.end_byte() =>
                {
                    return highlight;
                }
                HighlightEvent::HighlightStart(hl) => {
                    highlight = Some(hl);
                }
                _ => (),
            }
        }

        None
    }

    if event != PromptEvent::Validate {
        return Ok(());
    }

    let Some(highlight) = find_highlight_at_cursor(cx) else {
        return Ok(());
    };

    let content = cx.editor.theme.scope(highlight.0).to_string();

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                let content = ui::Markdown::new(content, editor.syn_loader.clone());
                let popup = Popup::new("hover", content).auto_close(true);
                compositor.replace_or_push("hover", popup);
            },
        ));
        Ok(call)
    };

    cx.jobs.callback(callback);

    Ok(())
}

fn vsplit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    } else if args.is_empty() {
        split(cx.editor, Action::VerticalSplit);
    } else {
        for arg in args {
            cx.editor.open(&PathBuf::from(arg), Action::VerticalSplit)?;
        }
    }
    Ok(())
}

fn hsplit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    } else if args.is_empty() {
        split(cx.editor, Action::HorizontalSplit);
    } else {
        for arg in args {
            cx.editor
                .open(&PathBuf::from(arg), Action::HorizontalSplit)?;
        }
    }
    Ok(())
}

fn vsplit_new(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    cx.editor.new_file(Action::VerticalSplit);
    Ok(())
}

fn hsplit_new(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    cx.editor.new_file(Action::HorizontalSplit);
    Ok(())
}

fn debug_eval(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if let Some(debugger) = cx.editor.debugger.as_mut() {
        let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
            (Some(frame), Some(thread_id)) => (frame, thread_id),
            _ => {
                bail!("Cannot find current stack frame to access variables")
            }
        };

        // TODO: support no frame_id
        let frame_id = debugger.stack_frames[&thread_id][frame].id;
        let expression = args.raw().to_string();

        let response = helix_lsp::block_on(debugger.eval(expression, Some(frame_id)))?;
        cx.editor.set_status(response.result);
    }
    Ok(())
}

fn debug_start(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    dap_start_impl(cx, args.next(), None, Some(args.map(Into::into).collect()))
}

fn debug_remote(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let address = args.next().map(|addr| addr.parse()).transpose()?;
    dap_start_impl(
        cx,
        args.next(),
        address,
        Some(args.map(Into::into).collect()),
    )
}

fn tutor(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let path = helix_loader::runtime_file(Path::new("tutor"));
    cx.editor.open(&path, Action::Replace)?;
    // Unset path to prevent accidentally saving to the original tutor file.
    doc_mut!(cx.editor).set_path(None);
    Ok(())
}

fn abort_goto_line_number_preview(cx: &mut compositor::Context) {
    if let Some(last_selection) = cx.editor.last_selection.take() {
        let scrolloff = cx.editor.config().scrolloff;

        let (view, doc) = current!(cx.editor);
        doc.set_selection(view.id, last_selection);
        view.ensure_cursor_in_view(doc, scrolloff);
    }
}

fn update_goto_line_number_preview(
    cx: &mut compositor::Context,
    mut args: Args,
) -> anyhow::Result<()> {
    cx.editor.last_selection.get_or_insert_with(|| {
        let (view, doc) = current!(cx.editor);
        doc.selection(view.id).clone()
    });

    let scrolloff = cx.editor.config().scrolloff;
    let line = args.next().unwrap().parse::<usize>()?;
    goto_line_without_jumplist(cx.editor, NonZeroUsize::new(line));

    let (view, doc) = current!(cx.editor);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

pub(super) fn goto_line_number(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    match event {
        PromptEvent::Abort => abort_goto_line_number_preview(cx),
        PromptEvent::Validate => {
            ensure!(!args.is_empty(), "Line number required");

            // If we are invoked directly via a keybinding, Validate is
            // sent without any prior Update events. Ensure the cursor
            // is moved to the appropriate location.
            update_goto_line_number_preview(cx, args)?;

            let last_selection = cx
                .editor
                .last_selection
                .take()
                .expect("update_goto_line_number_preview should always set last_selection");

            let (view, doc) = current!(cx.editor);
            view.jumps.push((doc.id(), last_selection));
        }

        // When a user hits backspace and there are no numbers left,
        // we can bring them back to their original selection. If they
        // begin typing numbers again, we'll start a new preview session.
        PromptEvent::Update if args.is_empty() => abort_goto_line_number_preview(cx),
        PromptEvent::Update => update_goto_line_number_preview(cx, args)?,
    }

    Ok(())
}

// Fetch the current value of a config option and output as status.
fn get_option(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.arg_count() != 1 {
        anyhow::bail!("Bad arguments. Usage: `:get key`");
    }

    let key = args.next().unwrap().to_lowercase();
    let key_error = || anyhow::anyhow!("Unknown key `{}`", key);

    let config = serde_json::json!(cx.editor.config().deref());
    let pointer = format!("/{}", key.replace('.', "/"));
    let value = config.pointer(&pointer).ok_or_else(key_error)?;

    cx.editor.set_status(value.to_string());
    Ok(())
}

/// Change config at runtime. Access nested values by dot syntax, for
/// example to disable smart case search, use `:set search.smart-case false`.
fn set_option(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let Some(key) = args.next().map(|arg| arg.to_lowercase()) else {
        anyhow::bail!("Bad arguments. Usage: `:set key field`, didn't provide `key`");
    };

    let field = args.rest();

    if field.is_empty() {
        anyhow::bail!("Bad arguments. Usage: `:set key field`, didn't provide `field`");
    }

    let mut config = serde_json::json!(&*cx.editor.config());
    let pointer = format!("/{}", key.replace('.', "/"));
    let value = config
        .pointer_mut(&pointer)
        .ok_or_else(|| anyhow::anyhow!("Unknown key `{key}`"))?;

    *value = if value.is_string() {
        // JSON strings require quotes, so we can't .parse() directly
        Value::String(field.to_string())
    } else {
        field
            .parse()
            .map_err(|err| anyhow::anyhow!("Could not parse field `{field}`: {err}"))?
    };

    let config = serde_json::from_value(config).expect(
        "`Config` was already deserialized, serialization is just a 'repacking' and should be valid",
    );

    cx.editor
        .config_events
        .0
        .send(ConfigEvent::Update(config))?;

    cx.editor
        .set_status(format!("'{key}' is now set to {field}"));

    Ok(())
}

/// Toggle boolean config option at runtime. Access nested values by dot
/// syntax.
/// Example:
/// -  `:toggle search.smart-case` (bool)
/// -  `:toggle line-number relative absolute` (string)
fn toggle_option(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.is_empty() {
        anyhow::bail!("Bad arguments. Usage: `:toggle key [values]?`");
    }
    let key = args.next().unwrap().to_lowercase();

    let mut config = serde_json::json!(&*cx.editor.config());
    let pointer = format!("/{}", key.replace('.', "/"));
    let value = config
        .pointer_mut(&pointer)
        .ok_or_else(|| anyhow::anyhow!("Unknown key `{}`", key))?;

    *value = match value {
        Value::Bool(ref value) => {
            ensure!(
                args.next().is_none(),
                "Bad arguments. For boolean configurations use: `:toggle key`"
            );
            Value::Bool(!value)
        }
        Value::String(ref value) => {
            ensure!(
                // key + arguments
                args.arg_count() >= 3,
                "Bad arguments. For string configurations use: `:toggle key val1 val2 ...`",
            );

            Value::String(
                args.clone()
                    .skip_while(|e| *e != value)
                    .nth(1)
                    .unwrap_or_else(|| args.nth(1).unwrap())
                    .to_string(),
            )
        }
        Value::Number(ref value) => {
            ensure!(
                // key + arguments
                args.arg_count() >= 3,
                "Bad arguments. For number configurations use: `:toggle key val1 val2 ...`",
            );

            let value = value.to_string();

            Value::Number(
                args.clone()
                    .skip_while(|e| *e != value)
                    .nth(1)
                    .unwrap_or_else(|| args.nth(1).unwrap())
                    .parse()?,
            )
        }
        Value::Array(value) => {
            let mut lists = serde_json::Deserializer::from_str(args.rest()).into_iter::<Value>();

            let (Some(first), Some(second)) =
                (lists.next().transpose()?, lists.next().transpose()?)
            else {
                anyhow::bail!(
                    "Bad arguments. For list configurations use: `:toggle key [...] [...]`",
                )
            };

            match (&first, &second) {
                (Value::Array(list), Value::Array(_)) => {
                    if list == value {
                        second
                    } else {
                        first
                    }
                }
                _ => anyhow::bail!("values must be lists"),
            }
        }
        Value::Null | Value::Object(_) => {
            anyhow::bail!("Configuration {key} does not support toggle yet")
        }
    };

    let status = format!("'{key}' is now set to {value}");
    let config = serde_json::from_value(config)?;

    cx.editor
        .config_events
        .0
        .send(ConfigEvent::Update(config))?;

    cx.editor.set_status(status);

    Ok(())
}

/// Change the language of the current buffer at runtime.
fn language(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if args.is_empty() {
        let doc = doc!(cx.editor);
        let language = doc.language_name().unwrap_or(DEFAULT_LANGUAGE_NAME);
        cx.editor.set_status(language.to_string());
        return Ok(());
    }

    if args.arg_count() != 1 {
        anyhow::bail!("Bad arguments. Usage: `:set-language language`");
    }

    let doc = doc_mut!(cx.editor);

    let language_id = args.next().unwrap();
    if language_id == DEFAULT_LANGUAGE_NAME {
        doc.set_language(None, None);
    } else {
        doc.set_language_by_language_id(language_id, cx.editor.syn_loader.clone())?;
    }
    doc.detect_indent_and_line_ending();

    let id = doc.id();
    cx.editor.refresh_language_servers(id);
    let doc = doc_mut!(cx.editor);
    let diagnostics =
        Editor::doc_diagnostics(&cx.editor.language_servers, &cx.editor.diagnostics, doc);
    doc.replace_diagnostics(diagnostics, &[], None);
    Ok(())
}

fn sort(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    sort_impl(cx, args, false)
}

fn sort_reverse(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    sort_impl(cx, args, true)
}

fn sort_impl(cx: &mut compositor::Context, _args: Args, reverse: bool) -> anyhow::Result<()> {
    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id);

    let mut fragments: Vec<_> = selection
        .slices(text)
        .map(|fragment| fragment.chunks().collect())
        .collect();

    fragments.sort_by(match reverse {
        true => |a: &Tendril, b: &Tendril| b.cmp(a),
        false => |a: &Tendril, b: &Tendril| a.cmp(b),
    });

    let transaction = Transaction::change(
        doc.text(),
        selection
            .into_iter()
            .zip(fragments)
            .map(|(s, fragment)| (s.from(), s.to(), Some(fragment))),
    );

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn reflow(cx: &mut compositor::Context, mut args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let cfg_text_width: usize = cx.editor.config().text_width;
    let (view, doc) = current!(cx.editor);

    // Find the text_width by checking the following sources in order:
    //   - The passed argument in `args`
    //   - The configured text-width for this language in languages.toml
    //   - The configured text-width in the config.toml
    let text_width: usize = args
        .next()
        .map(|num| num.parse::<usize>())
        .transpose()?
        .or_else(|| doc.language_config().and_then(|config| config.text_width))
        .unwrap_or(cfg_text_width);

    let rope = doc.text();

    let selection = doc.selection(view.id);
    let transaction = Transaction::change_by_selection(rope, selection, |range| {
        let fragment = range.fragment(rope.slice(..));
        let reflowed_text = helix_core::wrap::reflow_hard_wrap(&fragment, text_width);

        (range.from(), range.to(), Some(reflowed_text))
    });

    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

fn tree_sitter_subtree(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let (view, doc) = current!(cx.editor);

    if let Some(syntax) = doc.syntax() {
        let primary_selection = doc.selection(view.id).primary();
        let text = doc.text();
        let from = text.char_to_byte(primary_selection.from());
        let to = text.char_to_byte(primary_selection.to());
        if let Some(selected_node) = syntax.descendant_for_byte_range(from, to) {
            let mut contents = String::from("```tsq\n");
            helix_core::syntax::pretty_print_tree(&mut contents, selected_node)?;
            contents.push_str("\n```");

            let callback = async move {
                let call: job::Callback = Callback::EditorCompositor(Box::new(
                    move |editor: &mut Editor, compositor: &mut Compositor| {
                        let contents = ui::Markdown::new(contents, editor.syn_loader.clone());
                        let popup = Popup::new("hover", contents).auto_close(true);
                        compositor.replace_or_push("hover", popup);
                    },
                ));
                Ok(call)
            };

            cx.jobs.callback(callback);
        }
    }

    Ok(())
}

fn open_config(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    cx.editor
        .open(&helix_loader::config_file(), Action::Replace)?;
    Ok(())
}

fn open_workspace_config(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    cx.editor
        .open(&helix_loader::workspace_config_file(), Action::Replace)?;
    Ok(())
}

fn open_log(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    cx.editor.open(&helix_loader::log_file(), Action::Replace)?;
    Ok(())
}

fn refresh_config(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    cx.editor.config_events.0.send(ConfigEvent::Refresh)?;
    Ok(())
}

fn append_output(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(!args.is_empty(), "Shell command required");
    let cmd = helix_core::shellwords::unescape(args.raw());
    shell(cx, &cmd, &ShellBehavior::Append);
    Ok(())
}

fn insert_output(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(!args.is_empty(), "Shell command required");
    let cmd = helix_core::shellwords::unescape(args.raw());
    shell(cx, &cmd, &ShellBehavior::Insert);
    Ok(())
}

fn pipe_to(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    pipe_impl(cx, args, event, &ShellBehavior::Ignore)
}

fn pipe(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> anyhow::Result<()> {
    pipe_impl(cx, args, event, &ShellBehavior::Replace)
}

fn pipe_impl(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
    behavior: &ShellBehavior,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(!args.is_empty(), "Shell command required");
    let cmd = helix_core::shellwords::unescape(args.raw());
    shell(cx, &cmd, behavior);
    Ok(())
}

fn run_shell_command(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let shell = cx.editor.config().shell.clone();

    let args = helix_core::shellwords::unescape(args.raw()).into_owned();

    let callback = async move {
        let output = shell_impl_async(&shell, &args, None).await?;
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                if !output.is_empty() {
                    let contents = ui::Markdown::new(
                        format!("```sh\n{}\n```", output.trim_end()),
                        editor.syn_loader.clone(),
                    );
                    let popup = Popup::new("shell", contents).position(Some(
                        helix_core::Position::new(editor.cursor().0.unwrap_or_default().row, 2),
                    ));
                    compositor.replace_or_push("shell", popup);
                }
                editor.set_status("Command run");
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);

    Ok(())
}

fn reset_diff_change(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    ensure!(args.is_empty(), ":reset-diff-change takes no arguments");

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);
    let Some(handle) = doc.diff_handle() else {
        bail!("Diff is not available in the current buffer")
    };

    let diff = handle.load();
    let doc_text = doc.text().slice(..);
    let diff_base = diff.diff_base();
    let mut changes = 0;

    let transaction = Transaction::change(
        doc.text(),
        diff.hunks_intersecting_line_ranges(doc.selection(view.id).line_ranges(doc_text))
            .map(|hunk| {
                changes += 1;
                let start = diff_base.line_to_char(hunk.before.start as usize);
                let end = diff_base.line_to_char(hunk.before.end as usize);
                let text: Tendril = diff_base.slice(start..end).chunks().collect();
                (
                    doc_text.line_to_char(hunk.after.start as usize),
                    doc_text.line_to_char(hunk.after.end as usize),
                    (!text.is_empty()).then_some(text),
                )
            }),
    );
    if changes == 0 {
        bail!("There are no changes under any selection");
    }

    drop(diff); // make borrow check happy
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);
    cx.editor.set_status(format!(
        "Reset {changes} change{}",
        if changes == 1 { "" } else { "s" }
    ));
    Ok(())
}

fn clear_register(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    ensure!(
        args.arg_count() <= 1,
        ":clear-register takes at most 1 argument"
    );

    if args.is_empty() {
        cx.editor.registers.clear();
        cx.editor.set_status("All registers cleared");
        return Ok(());
    }

    let register = args.next().unwrap();

    ensure!(
        register.chars().count() == 1,
        format!("Invalid register {register}")
    );

    let register = register.chars().next().unwrap_or_default();
    if cx.editor.registers.remove(register) {
        cx.editor.set_status(format!("Register {register} cleared"));
    } else {
        cx.editor
            .set_error(format!("Register {register} not found"));
    }
    Ok(())
}

fn redraw(cx: &mut compositor::Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let callback = Box::pin(async move {
        let call: job::Callback =
            job::Callback::EditorCompositor(Box::new(|_editor, compositor| {
                compositor.need_full_redraw();
            }));

        Ok(call)
    });

    cx.jobs.callback(callback);

    Ok(())
}

fn move_buffer(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    ensure!(args.arg_count() == 1, format!(":move takes one argument"));

    let old_path = doc!(cx.editor)
        .path()
        .context("Scratch buffer cannot be moved. Use :write instead")?
        .clone();

    let new_path = args.next().unwrap();

    if let Err(err) = cx.editor.move_path(&old_path, new_path.as_ref()) {
        bail!("Could not move file: {err}");
    }
    Ok(())
}

fn yank_diagnostic(
    cx: &mut compositor::Context,
    mut args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let reg = match args.next() {
        Some(s) => {
            ensure!(s.chars().count() == 1, format!("Invalid register {s}"));
            s.chars().next().unwrap()
        }
        None => '+',
    };

    let (view, doc) = current_ref!(cx.editor);
    let primary = doc.selection(view.id).primary();

    // Look only for diagnostics that intersect with the primary selection
    let diag: Vec<_> = doc
        .diagnostics()
        .iter()
        .filter(|d| primary.overlaps(&helix_core::Range::new(d.range.start, d.range.end)))
        .map(|d| d.message.clone())
        .collect();
    let n = diag.len();
    if n == 0 {
        bail!("No diagnostics under primary selection");
    }

    cx.editor.registers.write(reg, diag)?;
    cx.editor.set_status(format!(
        "Yanked {n} diagnostic{} to register {reg}",
        if n == 1 { "" } else { "s" }
    ));
    Ok(())
}

fn read(cx: &mut compositor::Context, mut args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let scrolloff = cx.editor.config().scrolloff;
    let (view, doc) = current!(cx.editor);

    ensure!(!args.is_empty(), "file name is expected");
    ensure!(args.arg_count() == 1, "only the file name is expected");

    let filename = args.next().unwrap();
    let path = helix_stdx::path::expand_tilde(Path::new(filename));

    ensure!(
        path.exists() && path.is_file(),
        "path is not a file: {:?}",
        path
    );

    let file = std::fs::File::open(path).map_err(|err| anyhow!("error opening file: {}", err))?;
    let mut reader = BufReader::new(file);
    let (contents, _, _) = read_to_string(&mut reader, Some(doc.encoding()))
        .map_err(|err| anyhow!("error reading file: {}", err))?;
    let contents = Tendril::from(contents);
    let selection = doc.selection(view.id);
    let transaction = Transaction::insert(doc.text(), selection, contents);
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);

    Ok(())
}

pub const TYPABLE_COMMAND_LIST: &[TypableCommand] = &[
    TypableCommand {
        name: "quit",
        aliases: &["q"],
        doc: "Close the current view.",
        fun: quit,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "quit!",
        aliases: &["q!"],
        doc: "Force close the current view, ignoring unsaved changes.",
        fun: force_quit,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "open",
        aliases: &["o", "edit", "e"],
        doc: "Open a file from disk into the current view.",
        fun: open,
        signature: CommandSignature::all(completers::filename),
    },
    TypableCommand {
        name: "buffer-close",
        aliases: &["bc", "bclose"],
        doc: "Close the current buffer.",
        fun: buffer_close,
        signature: CommandSignature::all(completers::buffer),
    },
    TypableCommand {
        name: "buffer-close!",
        aliases: &["bc!", "bclose!"],
        doc: "Close the current buffer forcefully, ignoring unsaved changes.",
        fun: force_buffer_close,
        signature: CommandSignature::all(completers::buffer)
    },
    TypableCommand {
        name: "buffer-close-others",
        aliases: &["bco", "bcloseother"],
        doc: "Close all buffers but the currently focused one.",
        fun: buffer_close_others,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "buffer-close-others!",
        aliases: &["bco!", "bcloseother!"],
        doc: "Force close all buffers but the currently focused one.",
        fun: force_buffer_close_others,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "buffer-close-all",
        aliases: &["bca", "bcloseall"],
        doc: "Close all buffers without quitting.",
        fun: buffer_close_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "buffer-close-all!",
        aliases: &["bca!", "bcloseall!"],
        doc: "Force close all buffers ignoring unsaved changes without quitting.",
        fun: force_buffer_close_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "buffer-next",
        aliases: &["bn", "bnext"],
        doc: "Goto next buffer.",
        fun: buffer_next,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "buffer-previous",
        aliases: &["bp", "bprev"],
        doc: "Goto previous buffer.",
        fun: buffer_previous,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "write",
        aliases: &["w"],
        doc: "Write changes to disk. Accepts an optional path (:write some/path.txt)",
        fun: write,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "write!",
        aliases: &["w!"],
        doc: "Force write changes to disk creating necessary subdirectories. Accepts an optional path (:write! some/path.txt)",
        fun: force_write,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "write-buffer-close",
        aliases: &["wbc"],
        doc: "Write changes to disk and closes the buffer. Accepts an optional path (:write-buffer-close some/path.txt)",
        fun: write_buffer_close,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "write-buffer-close!",
        aliases: &["wbc!"],
        doc: "Force write changes to disk creating necessary subdirectories and closes the buffer. Accepts an optional path (:write-buffer-close! some/path.txt)",
        fun: force_write_buffer_close,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "new",
        aliases: &["n"],
        doc: "Create a new scratch buffer.",
        fun: new_file,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "format",
        aliases: &["fmt"],
        doc: "Format the file using an external formatter or language server.",
        fun: format,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "indent-style",
        aliases: &[],
        doc: "Set the indentation style for editing. ('t' for tabs or 1-16 for number of spaces.)",
        fun: set_indent_style,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "line-ending",
        aliases: &[],
        #[cfg(not(feature = "unicode-lines"))]
        doc: "Set the document's default line ending. Options: crlf, lf.",
        #[cfg(feature = "unicode-lines")]
        doc: "Set the document's default line ending. Options: crlf, lf, cr, ff, nel.",
        fun: set_line_ending,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "earlier",
        aliases: &["ear"],
        doc: "Jump back to an earlier point in edit history. Accepts a number of steps or a time span.",
        fun: earlier,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "later",
        aliases: &["lat"],
        doc: "Jump to a later point in edit history. Accepts a number of steps or a time span.",
        fun: later,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "write-quit",
        aliases: &["wq", "x"],
        doc: "Write changes to disk and close the current view. Accepts an optional path (:wq some/path.txt)",
        fun: write_quit,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "write-quit!",
        aliases: &["wq!", "x!"],
        doc: "Write changes to disk and close the current view forcefully. Accepts an optional path (:wq! some/path.txt)",
        fun: force_write_quit,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "write-all",
        aliases: &["wa"],
        doc: "Write changes from all buffers to disk.",
        fun: write_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "write-all!",
        aliases: &["wa!"],
        doc: "Forcefully write changes from all buffers to disk creating necessary subdirectories.",
        fun: force_write_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "write-quit-all",
        aliases: &["wqa", "xa"],
        doc: "Write changes from all buffers to disk and close all views.",
        fun: write_all_quit,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "write-quit-all!",
        aliases: &["wqa!", "xa!"],
        doc: "Write changes from all buffers to disk and close all views forcefully (ignoring unsaved changes).",
        fun: force_write_all_quit,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "quit-all",
        aliases: &["qa"],
        doc: "Close all views.",
        fun: quit_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "quit-all!",
        aliases: &["qa!"],
        doc: "Force close all views ignoring unsaved changes.",
        fun: force_quit_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "cquit",
        aliases: &["cq"],
        doc: "Quit with exit code (default 1). Accepts an optional integer exit code (:cq 2).",
        fun: cquit,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "cquit!",
        aliases: &["cq!"],
        doc: "Force quit with exit code (default 1) ignoring unsaved changes. Accepts an optional integer exit code (:cq! 2).",
        fun: force_cquit,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "theme",
        aliases: &[],
        doc: "Change the editor theme (show current theme if no name specified).",
        fun: theme,
        signature: CommandSignature::positional(&[completers::theme]),
    },
    TypableCommand {
        name: "yank-join",
        aliases: &[],
        doc: "Yank joined selections. A separator can be provided as first argument. Default value is newline.",
        fun: yank_joined,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "clipboard-yank",
        aliases: &[],
        doc: "Yank main selection into system clipboard.",
        fun: yank_main_selection_to_clipboard,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "clipboard-yank-join",
        aliases: &[],
        doc: "Yank joined selections into system clipboard. A separator can be provided as first argument. Default value is newline.", // FIXME: current UI can't display long doc.
        fun: yank_joined_to_clipboard,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "primary-clipboard-yank",
        aliases: &[],
        doc: "Yank main selection into system primary clipboard.",
        fun: yank_main_selection_to_primary_clipboard,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "primary-clipboard-yank-join",
        aliases: &[],
        doc: "Yank joined selections into system primary clipboard. A separator can be provided as first argument. Default value is newline.", // FIXME: current UI can't display long doc.
        fun: yank_joined_to_primary_clipboard,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "clipboard-paste-after",
        aliases: &[],
        doc: "Paste system clipboard after selections.",
        fun: paste_clipboard_after,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "clipboard-paste-before",
        aliases: &[],
        doc: "Paste system clipboard before selections.",
        fun: paste_clipboard_before,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "clipboard-paste-replace",
        aliases: &[],
        doc: "Replace selections with content of system clipboard.",
        fun: replace_selections_with_clipboard,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "primary-clipboard-paste-after",
        aliases: &[],
        doc: "Paste primary clipboard after selections.",
        fun: paste_primary_clipboard_after,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "primary-clipboard-paste-before",
        aliases: &[],
        doc: "Paste primary clipboard before selections.",
        fun: paste_primary_clipboard_before,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "primary-clipboard-paste-replace",
        aliases: &[],
        doc: "Replace selections with content of system primary clipboard.",
        fun: replace_selections_with_primary_clipboard,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "show-clipboard-provider",
        aliases: &[],
        doc: "Show clipboard provider name in status bar.",
        fun: show_clipboard_provider,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "change-current-directory",
        aliases: &["cd"],
        doc: "Change the current working directory.",
        fun: change_current_directory,
        signature: CommandSignature::positional(&[completers::directory]),
    },
    TypableCommand {
        name: "show-directory",
        aliases: &["pwd"],
        doc: "Show the current working directory.",
        fun: show_current_directory,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "encoding",
        aliases: &[],
        doc: "Set encoding. Based on `https://encoding.spec.whatwg.org`.",
        fun: set_encoding,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "character-info",
        aliases: &["char"],
        doc: "Get info about the character under the primary cursor.",
        fun: get_character_info,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "reload",
        aliases: &["rl"],
        doc: "Discard changes and reload from the source file.",
        fun: reload,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "reload-all",
        aliases: &["rla"],
        doc: "Discard changes and reload all documents from the source files.",
        fun: reload_all,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "update",
        aliases: &["u"],
        doc: "Write changes only if the file has been modified.",
        fun: update,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "lsp-workspace-command",
        aliases: &[],
        doc: "Open workspace command picker",
        fun: lsp_workspace_command,
        signature: CommandSignature::positional(&[completers::lsp_workspace_command]),
    },
    TypableCommand {
        name: "lsp-restart",
        aliases: &[],
        doc: "Restarts the language servers used by the current doc",
        fun: lsp_restart,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "lsp-stop",
        aliases: &[],
        doc: "Stops the language servers that are used by the current doc",
        fun: lsp_stop,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "tree-sitter-scopes",
        aliases: &[],
        doc: "Display tree sitter scopes, primarily for theming and development.",
        fun: tree_sitter_scopes,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "tree-sitter-highlight-name",
        aliases: &[],
        doc: "Display name of tree-sitter highlight scope under the cursor.",
        fun: tree_sitter_highlight_name,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "debug-start",
        aliases: &["dbg"],
        doc: "Start a debug session from a given template with given parameters.",
        fun: debug_start,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "debug-remote",
        aliases: &["dbg-tcp"],
        doc: "Connect to a debug adapter by TCP address and start a debugging session from a given template with given parameters.",
        fun: debug_remote,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "debug-eval",
        aliases: &[],
        doc: "Evaluate expression in current debug context.",
        fun: debug_eval,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "vsplit",
        aliases: &["vs"],
        doc: "Open the file in a vertical split.",
        fun: vsplit,
        signature: CommandSignature::all(completers::filename)
    },
    TypableCommand {
        name: "vsplit-new",
        aliases: &["vnew"],
        doc: "Open a scratch buffer in a vertical split.",
        fun: vsplit_new,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "hsplit",
        aliases: &["hs", "sp"],
        doc: "Open the file in a horizontal split.",
        fun: hsplit,
        signature: CommandSignature::all(completers::filename)
    },
    TypableCommand {
        name: "hsplit-new",
        aliases: &["hnew"],
        doc: "Open a scratch buffer in a horizontal split.",
        fun: hsplit_new,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "tutor",
        aliases: &[],
        doc: "Open the tutorial.",
        fun: tutor,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "goto",
        aliases: &["g"],
        doc: "Goto line number.",
        fun: goto_line_number,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "set-language",
        aliases: &["lang"],
        doc: "Set the language of current buffer (show current language if no value specified).",
        fun: language,
        signature: CommandSignature::positional(&[completers::language]),
    },
    TypableCommand {
        name: "set-option",
        aliases: &["set"],
        doc: "Set a config option at runtime.\nFor example to disable smart case search, use `:set search.smart-case false`.",
        fun: set_option,
        // TODO: Add support for completion of the options value(s), when appropriate.
        signature: CommandSignature::positional(&[completers::setting]),
    },
    TypableCommand {
        name: "toggle-option",
        aliases: &["toggle"],
        doc: "Toggle a boolean config option at runtime.\nFor example to toggle smart case search, use `:toggle search.smart-case`.",
        fun: toggle_option,
        signature: CommandSignature::positional(&[completers::setting]),
    },
    TypableCommand {
        name: "get-option",
        aliases: &["get"],
        doc: "Get the current value of a config option.",
        fun: get_option,
        signature: CommandSignature::positional(&[completers::setting]),
    },
    TypableCommand {
        name: "sort",
        aliases: &[],
        doc: "Sort ranges in selection.",
        fun: sort,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "rsort",
        aliases: &[],
        doc: "Sort ranges in selection in reverse order.",
        fun: sort_reverse,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "reflow",
        aliases: &[],
        doc: "Hard-wrap the current selection of lines to a given width.",
        fun: reflow,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "tree-sitter-subtree",
        aliases: &["ts-subtree"],
        doc: "Display the smallest tree-sitter subtree that spans the primary selection, primarily for debugging queries.",
        fun: tree_sitter_subtree,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "config-reload",
        aliases: &[],
        doc: "Refresh user config.",
        fun: refresh_config,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "config-open",
        aliases: &[],
        doc: "Open the user config.toml file.",
        fun: open_config,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "config-open-workspace",
        aliases: &[],
        doc: "Open the workspace config.toml file.",
        fun: open_workspace_config,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "log-open",
        aliases: &[],
        doc: "Open the helix log file.",
        fun: open_log,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "insert-output",
        aliases: &[],
        doc: "Run shell command, inserting output before each selection.",
        fun: insert_output,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "append-output",
        aliases: &[],
        doc: "Run shell command, appending output after each selection.",
        fun: append_output,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "pipe",
        aliases: &[],
        doc: "Pipe each selection to the shell command.",
        fun: pipe,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "pipe-to",
        aliases: &[],
        doc: "Pipe each selection to the shell command, ignoring output.",
        fun: pipe_to,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "run-shell-command",
        aliases: &["sh"],
        doc: "Run a shell command",
        fun: run_shell_command,
        signature: CommandSignature::all(completers::filename)
    },
    TypableCommand {
        name: "reset-diff-change",
        aliases: &["diffget", "diffg"],
        doc: "Reset the diff change at the cursor position.",
        fun: reset_diff_change,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "clear-register",
        aliases: &[],
        doc: "Clear given register. If no argument is provided, clear all registers.",
        fun: clear_register,
        signature: CommandSignature::all(completers::register),
    },
    TypableCommand {
        name: "redraw",
        aliases: &[],
        doc: "Clear and re-render the whole UI",
        fun: redraw,
        signature: CommandSignature::none(),
    },
    TypableCommand {
        name: "move",
        aliases: &["mv"],
        doc: "Move the current buffer and its corresponding file to a different path",
        fun: move_buffer,
        signature: CommandSignature::positional(&[completers::filename]),
    },
    TypableCommand {
        name: "yank-diagnostic",
        aliases: &[],
        doc: "Yank diagnostic(s) under primary cursor to register, or clipboard by default",
        fun: yank_diagnostic,
        signature: CommandSignature::all(completers::register),
    },
    TypableCommand {
        name: "read",
        aliases: &["r"],
        doc: "Load a file into buffer",
        fun: read,
        signature: CommandSignature::positional(&[completers::filename]),
    },
];

pub static TYPABLE_COMMAND_MAP: Lazy<HashMap<&'static str, &'static TypableCommand>> =
    Lazy::new(|| {
        TYPABLE_COMMAND_LIST
            .iter()
            .flat_map(|cmd| {
                std::iter::once((cmd.name, cmd))
                    .chain(cmd.aliases.iter().map(move |&alias| (alias, cmd)))
            })
            .collect()
    });

#[allow(clippy::unnecessary_unwrap, clippy::too_many_lines)]
pub(super) fn command_mode(cx: &mut Context) {
    let commands = cx.editor.config().commands.clone();
    let mut prompt = Prompt::new(
        ":".into(),
        Some(':'),
        // completion
        move |editor: &Editor, input: &str| {
            let shellwords = Shellwords::from(input.trim_start_matches('^'));
            let command = shellwords.command();

            let items = TYPABLE_COMMAND_LIST
                .iter()
                .map(|command| command.name)
                .chain(commands.names())
                // HACK: `to_string` because of lifetimes:
                //
                // captured variable cannot escape `FnMut` closure body
                // `FnMut` closures only have access to their captured variables while they are executing
                // therefore, they cannot allow references to captured variables to escape
                .map(|name| name.to_string());

            if command.is_empty()
                || (shellwords.args().next().is_none() && !shellwords.ends_with_whitespace())
            {
                fuzzy_match(command, items, false)
                    .into_iter()
                    .map(|(name, _)| (0.., name.into()))
                    .collect()
            } else {
                // Otherwise, use the command's completer and the last shellword
                // as completion input.
                let (word, len) = shellwords
                    .args()
                    .last()
                    .map_or(("", 0), |last| (last, last.len()));

                // TODO: Need to validate that the command name given for the completer option is a valid typable command
                let command = commands.get(command).map_or(command, |c| {
                    c.completer
                        .as_ref()
                        .map_or(command, |name| name.trim_start_matches(':'))
                });

                TYPABLE_COMMAND_MAP
                    .get(command)
                    .map(|tc| tc.completer_for_argument_number(argument_number_of(&shellwords)))
                    .map_or_else(Vec::new, |completer| {
                        completer(editor, word)
                            .into_iter()
                            .map(|(range, mut file)| {
                                file.content = shellwords::escape(file.content);

                                // offset ranges to input
                                let offset = input.len() - len;
                                let range = (range.start + offset)..;
                                (range, file)
                            })
                            .collect()
                    })
            }
        },
        // callback
        move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
            let shellwords = Shellwords::from(input);
            let command = shellwords.command();

            if command.is_empty() {
                return;
            }

            // If input is `:NUMBER`, interpret as line number and go there.
            if command.parse::<usize>().is_ok() {
                if let Err(err) = typed::goto_line_number(cx, Args::from(command), event) {
                    cx.editor.set_error(format!("{err}"));
                }
                return;
            }

            // Protects against weird recursion issues in expansion.
            // Positional arguments are only meant for use in the config.
            if contains_arg_variable(shellwords.args().raw()) {
                cx.editor.set_error(
                    "Input arguments cannot contain a positional argument variable: `%{arg[:INT]}`",
                );
            }

            let is_escaped = command.starts_with('^');
            let command = command.trim_start_matches('^');

            // Checking for custom commands first priotizes custom commands over built-in.
            //
            // Custom commands can be escaped with a `^`.
            //
            // TODO: When let chains are stable reduce nestedness
            if !is_escaped {
                if let Some(custom) = cx.editor.config().commands.get(command) {
                    for command in custom.iter() {
                        if let Some(typed_command) =
                            typed::TYPABLE_COMMAND_MAP.get(Shellwords::from(command).command())
                        {
                            // TODO: Expand variables: #11164
                            //
                            // let args = match variables::expand(...) {
                            //    Ok(args: Cow<'_, str>) => args,
                            //    Err(err) => {
                            //         cx.editor.set_error(format!("{err}"));
                            //         // Short circuit on error
                            //         return;
                            //    }
                            // }
                            //

                            // TEST: should allow for an option `%{arg}` even if no path is path is provided and work as if
                            // the `%{arg}` eas never present.
                            //
                            // Assume that if the command contains an `%{arg[:NUMBER]}` it will be accepting arguments from
                            // input and therefore not standalone.
                            //
                            // If `false`, then will use any arguments the command itself may have been written
                            // with and ignore any typed-in arguments.
                            //
                            // This is a special case for when config has simplest usage:
                            //
                            //     "ww" = ":write --force"
                            //
                            // It will only use: `--force` as arguments when running `:write`.
                            //
                            // This also means that users dont have to explicitly use `%{arg}` every time:
                            //
                            //     "ww" = ":write --force %{arg}"
                            //
                            // Though in the case of `:write`, they probably should.
                            //
                            // Regardless, some commands explicitly take zero arguments and this check should prevent
                            // input arguments being passed when they shouldnt.
                            //
                            // If `true`, then will assume that the command was passed arguments in expansion and that
                            // whats left is the full argument list to be sent run.
                            let args = if contains_arg_variable(command) {
                                // Input args
                                // TODO: Args::from(&args) from the expanded variables.
                                shellwords.args()
                            } else {
                                Shellwords::from(command).args()
                            };

                            if let Err(err) = (typed_command.fun)(cx, args, event) {
                                cx.editor.set_error(format!("{err}"));
                                // Short circuit on error
                                return;
                            }
                            // Handle static commands
                        } else if let Some(static_command) =
                            super::MappableCommand::STATIC_COMMAND_LIST
                                .iter()
                                .find(|mappable| {
                                    mappable.name() == Shellwords::from(command).command()
                                })
                        {
                            let mut cx = super::Context {
                                register: None,
                                count: None,
                                editor: cx.editor,
                                callback: vec![],
                                on_next_key_callback: None,
                                jobs: cx.jobs,
                            };

                            let MappableCommand::Static { fun, .. } = static_command else {
                                unreachable!("should only be able to get a static command from `STATIC_COMMAND_LIST`")
                            };

                            (fun)(&mut cx);
                            // Handle macro
                        } else if let Some(suffix) = command.strip_prefix('@') {
                            let keys = match helix_view::input::parse_macro(suffix) {
                                Ok(keys) => keys,
                                Err(err) => {
                                    cx.editor.set_error(format!(
                                        "failed to parse macro `{command}`: {err}"
                                    ));
                                    return;
                                }
                            };

                            // Protect against recursive macros.
                            if cx.editor.macro_replaying.contains(&'@') {
                                cx.editor.set_error("Cannot execute macro because the [@] register is already playing a macro");
                                return;
                            }

                            let mut cx = super::Context {
                                register: None,
                                count: None,
                                editor: cx.editor,
                                callback: vec![],
                                on_next_key_callback: None,
                                jobs: cx.jobs,
                            };

                            cx.editor.macro_replaying.push('@');
                            cx.callback.push(Box::new(move |compositor, cx| {
                                for key in keys {
                                    compositor.handle_event(&compositor::Event::Key(key), cx);
                                }
                                cx.editor.macro_replaying.pop();
                            }));
                        } else if event == PromptEvent::Validate {
                            cx.editor.set_error(format!("no such command: '{command}'"));
                            // Short circuit on error
                            return;
                        }
                    }
                } else if let Some(cmd) = typed::TYPABLE_COMMAND_MAP.get(command) {
                    if let Err(err) = (cmd.fun)(cx, shellwords.args(), event) {
                        cx.editor.set_error(format!("{err}"));
                    }
                }
            }
            // Handle typable commands as normal
            else if let Some(cmd) = typed::TYPABLE_COMMAND_MAP.get(command) {
                if let Err(err) = (cmd.fun)(cx, shellwords.args(), event) {
                    cx.editor.set_error(format!("{err}"));
                }
            } else if event == PromptEvent::Validate {
                cx.editor.set_error(format!("no such command: '{command}'"));
            }
        },
    );

    let commands = cx.editor.config().commands.clone();
    prompt.doc_fn = Box::new(move |input: &str| {
        let shellwords = Shellwords::from(input);

        if let Some(command) = commands.get(input) {
            return Some(command.prompt().into());
        } else if let Some(typed::TypableCommand { doc, aliases, .. }) =
            typed::TYPABLE_COMMAND_MAP.get(shellwords.command())
        {
            if aliases.is_empty() {
                return Some((*doc).into());
            }

            return Some(format!("{}\nAliases: {}", doc, aliases.join(", ")).into());
        }

        None
    });

    // Calculate initial completion
    prompt.recalculate_completion(cx.editor);
    cx.push_layer(Box::new(prompt));
}

fn argument_number_of(shellwords: &Shellwords) -> usize {
    shellwords
        .args()
        .arg_count()
        .saturating_sub(1 - usize::from(shellwords.ends_with_whitespace()))
}

// TODO: Will turn into check for an input argument so that it cannot be `%{arg}`
// as this could cause issues with expansion recursion
// NOTE: Only used in one
#[inline(always)]
fn contains_arg_variable(command: &str) -> bool {
    let mut idx = 0;
    let bytes = command.as_bytes();

    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            if let Some(next) = bytes.get(idx + 1) {
                if *next == b'{' {
                    // advance beyond `{`
                    idx += 2;
                    if let Some(arg) = bytes.get(idx..) {
                        match arg {
                            [b'a', b'r', b'g', b':', ..] => {
                                // Advance beyond the `:`
                                idx += 4;

                                let mut is_prev_digit = false;

                                for byte in &bytes[idx..] {
                                    // Found end of arg bracket
                                    if *byte == b'}' && is_prev_digit {
                                        return true;
                                    }

                                    if char::from(*byte).is_ascii_digit() {
                                        is_prev_digit = true;
                                    } else {
                                        break;
                                    }
                                }
                            }
                            [b'a', b'r', b'g', b'}', ..] => {
                                return true;
                            }
                            _ => {
                                idx += 2 + 4;
                                continue;
                            }
                        }
                    }
                    idx += 1;
                    continue;
                }
                idx += 1;
                continue;
            }
            idx += 1;
            continue;
        }
        idx += 1;
        continue;
    }

    false
}

#[test]
fn test_argument_number_of() {
    let cases = vec![
        ("set-option", 0),
        ("set-option ", 0),
        ("set-option a", 0),
        ("set-option asdf", 0),
        ("set-option asdf ", 1),
        ("set-option asdf xyz", 1),
        ("set-option asdf xyz abc", 2),
        ("set-option asdf xyz abc ", 3),
    ];

    for case in cases {
        assert_eq!(case.1, argument_number_of(&Shellwords::from(case.0)));
    }
}

#[test]
fn should_indicate_if_command_contained_arg_variable() {
    assert!(!contains_arg_variable("write --force"));

    // Must provide digit
    assert!(!contains_arg_variable("write --force %{arg:}"));

    // Muts have `:` before digits can be added
    assert!(!contains_arg_variable("write --force %{arg122444}"));

    // Must have closing bracket
    assert!(!contains_arg_variable("write --force %{arg"));
    assert!(!contains_arg_variable("write --force %{arg:1"));

    // Has valid variable
    assert!(contains_arg_variable("write --force %{arg}"));
    assert!(contains_arg_variable("write --force %{arg:1083472348978}"));
}
