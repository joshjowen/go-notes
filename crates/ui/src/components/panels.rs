//! The sidebar panes: tabs, search, tags, backlinks and the outline.

use go_notes_shared::{SearchHit, SNIPPET_CLOSE, SNIPPET_OPEN};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::state::use_app;

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

#[component]
pub fn TabBar() -> impl IntoView {
    let state = use_app();

    view! {
        <div class="gn-tabs">
            {move || {
                state
                    .tabs
                    .get()
                    .into_iter()
                    .enumerate()
                    .map(|(index, tab)| {
                        let is_active = move || state.active.get() == Some(index);
                        let title = tab.title.clone();
                        let path = tab.path.clone();
                        let dirty = tab.dirty;

                        view! {
                            <div
                                class="gn-tab"
                                class:gn-active=is_active
                                title=path
                                on:click=move |_| state.active.set(Some(index))
                                on:auxclick=move |ev| {
                                    // Middle click closes, as in every browser and editor.
                                    if ev.button() == 1 {
                                        ev.prevent_default();
                                        state.close_tab(index);
                                    }
                                }
                            >
                                <span class="gn-tab-title">{title}</span>
                                {if dirty {
                                    view! {
                                        <span class="gn-tab-dirty" title="Unsaved changes"></span>
                                    }
                                        .into_any()
                                } else {
                                    view! {
                                        <button
                                            class="gn-tab-close"
                                            title="Close"
                                            on:click=move |ev| {
                                                ev.stop_propagation();
                                                state.close_tab(index);
                                            }
                                        >
                                            "✕"
                                        </button>
                                    }
                                        .into_any()
                                }}
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[component]
pub fn SearchPane() -> impl IntoView {
    let state = use_app();
    let query = RwSignal::new(String::new());
    let hits = RwSignal::new(Vec::<SearchHit>::new());
    let searching = RwSignal::new(false);

    let run = move || {
        let text = query.get_untracked().trim().to_string();
        if text.is_empty() {
            hits.set(Vec::new());
            return;
        }
        searching.set(true);
        spawn_local(async move {
            match api::search(text).await {
                Ok(response) => hits.set(response.hits),
                Err(err) => state.error(err.user_message()),
            }
            searching.set(false);
        });
    };

    view! {
        <div class="gn-panel-section">
            <input
                class="gn-search-input"
                type="search"
                placeholder="Search all notes…"
                autocomplete="off"
                prop:value=move || query.get()
                on:input=move |ev| query.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" {
                        run();
                    }
                }
            />
        </div>

        <div class="gn-pane-body">
            {move || {
                if searching.get() {
                    return view! { <p class="gn-empty">"Searching…"</p> }.into_any();
                }
                let results = hits.get();
                if results.is_empty() {
                    let message = if query.get().trim().is_empty() {
                        "Type a search and press Enter. Whole words match first; partial words and \
                         near misses are found as a fallback."
                    } else {
                        "Nothing matched."
                    };
                    return view! { <p class="gn-empty">{message}</p> }.into_any();
                }

                view! {
                    <div class="gn-panel-section">
                        {results
                            .into_iter()
                            .map(|hit| {
                                let path = hit.path.clone();
                                let title = hit.title.clone();
                                view! {
                                    <button
                                        class="gn-search-hit"
                                        on:click=move |_| {
                                            state.open_tab(path.clone(), title.clone());
                                        }
                                    >
                                        <span class="gn-backlink-title">{hit.title.clone()}</span>
                                        <span class="gn-backlink-context">
                                            {highlight(&hit.snippet)}
                                        </span>
                                    </button>
                                }
                            })
                            .collect_view()}
                    </div>
                }
                    .into_any()
            }}
        </div>
    }
}

/// Turns the server's `«…»` markers into `<mark>` elements.
///
/// The server deliberately does not send HTML — it delimits matches with two
/// characters that cannot appear in its own output, and the client decides how
/// to render them. That way a note containing `<script>` is still just text.
fn highlight(snippet: &str) -> impl IntoView {
    let mut parts = Vec::new();
    let mut inside = false;

    for segment in snippet.split(|c| c == SNIPPET_OPEN || c == SNIPPET_CLOSE) {
        if !segment.is_empty() {
            let text = segment.to_string();
            parts.push(if inside {
                view! { <mark>{text}</mark> }.into_any()
            } else {
                view! { <span>{text}</span> }.into_any()
            });
        }
        inside = !inside;
    }
    parts
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

#[component]
pub fn TagPane() -> impl IntoView {
    let state = use_app();
    let tags = RwSignal::new(Vec::new());
    let selected = RwSignal::new(None::<String>);
    let tagged = RwSignal::new(Vec::new());

    Effect::new(move |_| {
        // Refetched whenever a save might have changed the tag set.
        let _ = state.tree_epoch.get();
        spawn_local(async move {
            if let Ok(found) = api::tags().await {
                tags.set(found);
            }
        });
    });

    view! {
        <div class="gn-pane-body">
            {move || {
                let all = tags.get();
                if all.is_empty() {
                    return view! {
                        <p class="gn-empty">
                            "No tags yet. Write " <code>"#like-this"</code>
                            " in a note, or add a " <code>"tags:"</code> " list to its frontmatter."
                        </p>
                    }
                        .into_any();
                }

                view! {
                    <div class="gn-panel-section">
                        {all
                            .into_iter()
                            .map(|tag| {
                                let name = tag.name.clone();
                                view! {
                                    <button
                                        class="gn-tag-row"
                                        on:click=move |_| {
                                            let name = name.clone();
                                            selected.set(Some(name.clone()));
                                            spawn_local(async move {
                                                if let Ok(notes) = api::notes_with_tag(name).await {
                                                    tagged.set(notes);
                                                }
                                            });
                                        }
                                    >
                                        <span>{format!("#{}", tag.name)}</span>
                                        <span class="gn-tag-count">{tag.count}</span>
                                    </button>
                                }
                            })
                            .collect_view()}
                    </div>
                }
                    .into_any()
            }}

            <Show when=move || selected.get().is_some()>
                <div class="gn-panel-section">
                    <p class="gn-panel-title">
                        {move || format!("#{}", selected.get().unwrap_or_default())}
                    </p>
                    {move || {
                        tagged
                            .get()
                            .into_iter()
                            .map(|item| {
                                let path = item.path.clone();
                                let title = item.title.clone();
                                view! {
                                    <button
                                        class="gn-outline-item"
                                        on:click=move |_| {
                                            state.open_tab(path.clone(), title.clone());
                                        }
                                    >
                                        {item.title.clone()}
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Show>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Backlinks and outline
// ---------------------------------------------------------------------------

#[component]
pub fn BacklinksPane() -> impl IntoView {
    let state = use_app();

    view! {
        <div class="gn-pane-body">
            <div class="gn-panel-section">
                <p class="gn-panel-title">"Linked mentions"</p>
                {move || {
                    let links = state.backlinks.get();
                    if links.is_empty() {
                        return view! {
                            <p class="gn-empty">
                                "Nothing links here yet. Type " <code>"[["</code>
                                " in another note to make a connection."
                            </p>
                        }
                            .into_any();
                    }

                    links
                        .into_iter()
                        .map(|link| {
                            let path = link.path.clone();
                            let title = link.title.clone();
                            view! {
                                <button
                                    class="gn-backlink"
                                    on:click=move |_| {
                                        state.open_tab(path.clone(), title.clone());
                                    }
                                >
                                    <span class="gn-backlink-title">{link.title.clone()}</span>
                                    <span class="gn-backlink-context">{link.context.clone()}</span>
                                </button>
                            }
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
        </div>
    }
}

/// Headings in the active note, built from the markdown the editor holds.
#[component]
pub fn OutlinePane(headings: Memo<Vec<(usize, String)>>) -> impl IntoView {
    view! {
        <div class="gn-pane-body">
            <div class="gn-panel-section">
                <p class="gn-panel-title">"Outline"</p>
                {move || {
                    let items = headings.get();
                    if items.is_empty() {
                        return view! {
                            <p class="gn-empty">"This note has no headings."</p>
                        }
                            .into_any();
                    }
                    items
                        .into_iter()
                        .map(|(level, text)| {
                            let indent = format!("padding-left: {}px", 6 + (level - 1) * 12);
                            view! { <div class="gn-outline-item" style=indent>{text}</div> }
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
        </div>
    }
}

/// Extracts ATX headings, ignoring anything inside a fenced code block.
///
/// A `#` inside a shell snippet is a comment, not a heading, and an outline that
/// listed them would be worse than no outline at all.
pub fn extract_headings(markdown: &str) -> Vec<(usize, String)> {
    let mut headings = Vec::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('#') {
            continue;
        }

        let level = trimmed.chars().take_while(|c| *c == '#').count();
        // Seven or more hashes is not a heading in CommonMark, and a heading
        // needs a space after the hashes.
        if !(1..=6).contains(&level) {
            continue;
        }
        let rest = &trimmed[level..];
        if !rest.starts_with(' ') {
            continue;
        }
        let text = rest.trim().trim_end_matches('#').trim();
        if !text.is_empty() {
            headings.push((level, text.to_string()));
        }
    }
    headings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_headings_by_level() {
        let markdown = "# One\n\ntext\n\n## Two\n\n### Three\n";
        assert_eq!(
            extract_headings(markdown),
            vec![
                (1, "One".to_string()),
                (2, "Two".to_string()),
                (3, "Three".to_string())
            ]
        );
    }

    /// The case that makes a naive implementation embarrassing: shell comments
    /// in a code block are not headings.
    #[test]
    fn ignores_hashes_inside_code_fences() {
        let markdown = "# Real\n\n```sh\n# not a heading\n## also not\n```\n\n## Also real\n";
        assert_eq!(
            extract_headings(markdown),
            vec![(1, "Real".to_string()), (2, "Also real".to_string())]
        );
    }

    #[test]
    fn requires_a_space_after_the_hashes() {
        assert!(extract_headings("#nospace\n").is_empty());
        assert!(extract_headings("####### too many\n").is_empty());
        assert_eq!(extract_headings("# yes\n"), vec![(1, "yes".to_string())]);
    }

    #[test]
    fn strips_closing_hashes() {
        assert_eq!(
            extract_headings("## Closed ##\n"),
            vec![(2, "Closed".to_string())]
        );
    }

    #[test]
    fn tolerates_an_unclosed_fence() {
        // An unterminated fence means everything after it is code; no headings.
        assert_eq!(
            extract_headings("# Before\n\n```\n# inside\n"),
            vec![(1, "Before".to_string())]
        );
    }
}
