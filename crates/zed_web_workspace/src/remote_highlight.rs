//! Apply server-side syntax highlights to open editors.
//!
//! The browser build does not ship tree-sitter grammars. Instead the remote
//! server tokenizes buffer text (`Highlight::document`) and returns spans
//! keyed by Zed SyntaxTheme capture names (`keyword`, `string`, …). We map
//! those to the active theme and paint them with `Editor::highlight_text`.

use collections::HashMap;
use editor::{Editor, HighlightKey};
use gpui::{App, Context, Entity, HighlightStyle, WeakEntity};
use language::{Buffer, BufferEvent};
use multi_buffer::{Anchor, MultiBufferOffset};
use std::ops::Range;
use std::time::Duration;
use theme::ActiveTheme;
use wasm_remote::RemoteClient;

/// Highlight key namespace for remote syntax spans.
/// Different `kind` strings map to distinct `SyntaxTreeView(n)` slots so each
/// token type can have its own `HighlightStyle`.
fn kind_key(kind: &str) -> HighlightKey {
    let id = match kind {
        "keyword" => 1,
        "string" | "string.escape" => 2,
        "comment" | "comment.doc" => 3,
        "function" | "function.method" | "function.definition" => 4,
        "type" => 5,
        "number" => 6,
        "operator" => 7,
        "punctuation" => 8,
        "constant" | "boolean" => 9,
        "attribute" | "property" => 10,
        "variable" => 11,
        "tag" => 12,
        "title" => 13,
        "preproc" => 14,
        "label" => 15,
        "emphasis" | "emphasis.strong" => 16,
        other => {
            let mut h: u32 = 2166136261;
            for b in other.bytes() {
                h ^= b as u32;
                h = h.wrapping_mul(16777619);
            }
            100 + (h % 800) as usize
        }
    };
    HighlightKey::SyntaxTreeView(id)
}

fn style_for_kind(kind: &str, cx: &App) -> HighlightStyle {
    let syntax = cx.theme().syntax();
    if let Some(style) = syntax.style_for_name(kind) {
        return style;
    }
    if let Some((parent, _)) = kind.split_once('.') {
        if let Some(style) = syntax.style_for_name(parent) {
            return style;
        }
    }
    match kind {
        "keyword" | "preproc" => HighlightStyle {
            color: Some(gpui::hsla(280. / 360., 0.6, 0.7, 1.)),
            ..Default::default()
        },
        "string" | "string.escape" => HighlightStyle {
            color: Some(gpui::hsla(100. / 360., 0.5, 0.65, 1.)),
            ..Default::default()
        },
        "comment" | "comment.doc" => HighlightStyle {
            color: Some(gpui::hsla(0., 0., 0.5, 1.)),
            font_style: Some(gpui::FontStyle::Italic),
            ..Default::default()
        },
        "function" | "function.method" | "function.definition" => HighlightStyle {
            color: Some(gpui::hsla(210. / 360., 0.7, 0.7, 1.)),
            ..Default::default()
        },
        "type" => HighlightStyle {
            color: Some(gpui::hsla(180. / 360., 0.5, 0.65, 1.)),
            ..Default::default()
        },
        "number" | "constant" | "boolean" => HighlightStyle {
            color: Some(gpui::hsla(30. / 360., 0.7, 0.65, 1.)),
            ..Default::default()
        },
        _ => HighlightStyle::default(),
    }
}

#[derive(serde::Deserialize)]
struct HighlightSpan {
    start: usize,
    end: usize,
    kind: String,
}

#[derive(serde::Deserialize)]
struct HighlightResponse {
    spans: Vec<HighlightSpan>,
    #[serde(default)]
    #[allow(dead_code)]
    language: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Install observers that request server highlights for every new editor.
pub fn install(remote: RemoteClient, cx: &mut App) {
    cx.observe_new(move |editor: &mut Editor, window, cx| {
        let Some(window) = window else {
            return;
        };
        let remote = remote.clone();
        let editor_entity = cx.entity();
        let buffer = editor.buffer().read(cx).as_singleton();

        schedule_highlight(editor_entity.downgrade(), remote.clone(), cx);

        if let Some(buffer) = buffer {
            cx.subscribe_in(&buffer, window, {
                let remote = remote.clone();
                let editor_entity = editor_entity.clone();
                move |_editor, _buffer, event, _window, cx| match event {
                    BufferEvent::Edited { .. }
                    | BufferEvent::Operation { .. }
                    | BufferEvent::Reloaded
                    | BufferEvent::LanguageChanged(_) => {
                        schedule_highlight(editor_entity.downgrade(), remote.clone(), cx);
                    }
                    _ => {}
                }
            })
            .detach();
        }
    })
    .detach();

    web_sys::console::log_1(
        &"zed_web_workspace: remote syntax highlighting installed (native server tokenizer)".into(),
    );
}

fn schedule_highlight(editor: WeakEntity<Editor>, remote: RemoteClient, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(120))
            .await;

        let Some((path, language, text, buffer_entity)) = editor
            .update(cx, |editor, cx| {
                let multi = editor.buffer().read(cx);
                let buffer = multi.as_singleton()?;
                let buffer_read = buffer.read(cx);
                let text = buffer_read.snapshot().text();
                let language = buffer_read.language().map(|l| l.name().lsp_id());
                let path = buffer_read.file().map(|f| {
                    if let Some(local) = f.as_local() {
                        local.abs_path(cx).to_string_lossy().into_owned()
                    } else {
                        // Virtual /workspace path for RemoteFs
                        format!("/workspace/{}", f.path().as_unix_str())
                    }
                })?;
                Some((path, language, text, buffer.clone()))
            })
            .ok()
            .flatten()
        else {
            return;
        };

        let response: Result<HighlightResponse, _> = remote
            .call(
                "Highlight::document",
                &serde_json::json!({
                    "path": path,
                    "text": text,
                    "language": language,
                }),
            )
            .await;

        let Ok(response) = response else {
            web_sys::console::warn_1(
                &format!("zed_web_workspace: highlight request failed for {path}").into(),
            );
            return;
        };
        if let Some(err) = response.error.as_ref() {
            web_sys::console::warn_1(&format!("zed_web_workspace: highlight error: {err}").into());
        }

        let _ = editor.update(cx, |editor, cx| {
            apply_spans(editor, &buffer_entity, &response.spans, cx);
        });
    })
    .detach();
}

fn apply_spans(
    editor: &mut Editor,
    _buffer: &Entity<Buffer>,
    spans: &[HighlightSpan],
    cx: &mut Context<Editor>,
) {
    editor.clear_highlights_with(
        &mut |key| matches!(key, HighlightKey::SyntaxTreeView(_)),
        cx,
    );

    if spans.is_empty() {
        cx.notify();
        return;
    }

    // Singleton multi-buffer offsets match the underlying buffer byte offsets.
    let multi_snapshot = editor.buffer().read(cx).snapshot(cx);
    let len = multi_snapshot.len().0;

    let mut by_kind: HashMap<String, Vec<Range<Anchor>>> = HashMap::default();
    for span in spans {
        if span.start >= span.end || span.start >= len {
            continue;
        }
        let end = span.end.min(len);
        let range = multi_snapshot.anchor_before(MultiBufferOffset(span.start))
            ..multi_snapshot.anchor_before(MultiBufferOffset(end));
        by_kind.entry(span.kind.clone()).or_default().push(range);
    }

    for (kind, ranges) in by_kind {
        let style = style_for_kind(&kind, cx);
        if style == HighlightStyle::default()
            && matches!(kind.as_str(), "punctuation" | "operator" | "text")
        {
            continue;
        }
        editor.highlight_text(kind_key(&kind), ranges, style, cx);
    }
    cx.notify();
}
