//! Search in a preview of text: a regular expression, as grep takes them, or plain text, over
//! what the preview shows. Matches are highlighted with the CSS Custom Highlight API, which
//! marks text ranges without touching the DOM, so syntax highlighting stays as it is; where the
//! API is missing, the current match is selected instead.

use leptos::{ev, html, prelude::*};
use regex::{Regex, RegexBuilder};
use wasm_bindgen::{JsCast, JsValue, prelude::*};
use web_sys::{HtmlElement, Node, Range};

/// The most matches highlighted.
const MATCHES_MAX: usize = 10_000;
/// The highlights' names, styled in style.css as `::highlight(...)`.
const ALL: &str = "preview-search";
const CURRENT: &str = "preview-search-current";

#[wasm_bindgen(inline_js = "
export function textNodes(root) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const nodes = [];
  while (walker.nextNode()) nodes.push(walker.currentNode);
  return nodes;
}
export function highlight(name, ranges) {
  if (!globalThis.CSS?.highlights || !globalThis.Highlight) return false;
  if (!ranges.length) { CSS.highlights.delete(name); return true; }
  const highlight = new Highlight();
  for (const range of ranges) highlight.add(range);
  CSS.highlights.set(name, highlight);
  return true;
}
export function select(range) {
  const selection = getSelection();
  selection?.removeAllRanges();
  if (range) selection?.addRange(range);
}
export function reveal(range, scroller) {
  const r = range.getBoundingClientRect(), s = scroller.getBoundingClientRect();
  if (r.top < s.top + 70 || r.bottom > s.bottom - 20) scroller.scrollTop += r.top - s.top - s.height / 3;
  if (r.left < s.left || r.right > s.right) scroller.scrollLeft += r.left - s.left - s.width / 3;
}
export function watch(root, changed) {
  let frame;
  const observer = new MutationObserver(() => { cancelAnimationFrame(frame); frame = requestAnimationFrame(changed); });
  observer.observe(root, { childList: true, subtree: true, characterData: true });
  return () => { cancelAnimationFrame(frame); observer.disconnect(); };
}
")]
extern "C" {
    #[wasm_bindgen(js_name = textNodes)]
    fn text_nodes(root: &Node) -> js_sys::Array;
    /// Whether the browser highlights ranges; false where it cannot.
    fn highlight(name: &str, ranges: &js_sys::Array) -> bool;
    fn select(range: Option<&Range>);
    /// Scroll `scroller` until `range` shows.
    fn reveal(range: &Range, scroller: &HtmlElement);
    /// Call `changed` once a frame while `root`'s content changes; returns what stops it.
    fn watch(root: &Node, changed: &js_sys::Function) -> js_sys::Function;
}

/// What a search looks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub regex: bool,
    pub match_case: bool,
}
impl Query {
    /// The pattern, or why it is not one.
    pub fn pattern(&self) -> Result<Regex, String> {
        let source = if self.regex {
            self.text.clone()
        } else {
            regex::escape(&self.text)
        };
        RegexBuilder::new(&source)
            .case_insensitive(!self.match_case)
            // ^ and $ match at each line, as in an editor.
            .multi_line(true)
            .size_limit(1 << 22)
            .build()
            .map_err(|e| match e {
                regex::Error::Syntax(message) => message
                    .lines()
                    .last()
                    .unwrap_or("invalid regular expression")
                    .trim_start_matches("error: ")
                    .to_string(),
                other => other.to_string(),
            })
    }
}

/// Where the matches of `pattern` are in `pieces`, the texts of consecutive DOM text nodes, as
/// (piece, UTF-16 offset) for each start and end, since DOM offsets count UTF-16 units. Empty
/// matches are left out; at most `limit` are found.
pub fn locate(
    pieces: &[String],
    pattern: &Regex,
    limit: usize,
) -> Vec<((usize, u32), (usize, u32))> {
    let text: String = pieces.concat();
    let mut starts = Vec::with_capacity(pieces.len() + 1);
    let mut at = 0;
    for piece in pieces {
        starts.push(at);
        at += piece.len();
    }
    starts.push(at);
    // Matches come in order, so two cursors walk the text once: a start at the end of a piece is
    // at the start of the next, an end at the start of a piece is at the end of the previous.
    struct Cursor {
        piece: usize,
        byte: usize,
        utf16: u32,
    }
    let advance = |cursor: &mut Cursor, to: usize, end: bool| {
        while cursor.piece + 1 < pieces.len()
            && (to > starts[cursor.piece + 1] || (!end && to == starts[cursor.piece + 1]))
        {
            cursor.piece += 1;
            cursor.byte = starts[cursor.piece];
            cursor.utf16 = 0;
        }
        cursor.utf16 += text[cursor.byte..to].encode_utf16().count() as u32;
        cursor.byte = to;
        (cursor.piece, cursor.utf16)
    };
    let (mut start, mut end) = (
        Cursor {
            piece: 0,
            byte: 0,
            utf16: 0,
        },
        Cursor {
            piece: 0,
            byte: 0,
            utf16: 0,
        },
    );
    pattern
        .find_iter(&text)
        .filter(|m| !m.is_empty())
        .take(limit)
        .map(|m| {
            (
                advance(&mut start, m.start(), false),
                advance(&mut end, m.end(), true),
            )
        })
        .collect()
}

/// The ranges `pattern` matches in `root`'s text.
fn find(root: &Node, pattern: &Regex) -> Vec<Range> {
    let nodes: Vec<Node> = text_nodes(root)
        .iter()
        .map(|n| n.unchecked_into::<Node>())
        .collect();
    let pieces: Vec<String> = nodes
        .iter()
        .map(|n| n.node_value().unwrap_or_default())
        .collect();
    let document = document();
    locate(&pieces, pattern, MATCHES_MAX)
        .into_iter()
        .filter_map(|((s, so), (e, eo))| {
            let range = document.create_range().ok()?;
            range.set_start(&nodes[s], so).ok()?;
            range.set_end(&nodes[e], eo).ok()?;
            Some(range)
        })
        .collect()
}

/// The matches found, and the one shown.
#[derive(Default)]
struct Found {
    ranges: Vec<Range>,
    current: usize,
}

/// A preview of text that can be searched, by the viewer bar's search button or Ctrl/Cmd+F.
#[component]
pub fn Searchable(children: Children) -> impl IntoView {
    let open = RwSignal::new(false);
    let text = RwSignal::new(String::new());
    let regex = RwSignal::new(true);
    let match_case = RwSignal::new(false);
    let status = RwSignal::new(String::new());
    let invalid = RwSignal::new(false);
    let content = NodeRef::<html::Div>::new();
    let field = NodeRef::<html::Input>::new();
    let found = StoredValue::new_local(Found::default());

    let scroller = || {
        document()
            .get_element_by_id("viewer-content")
            .map(|e| e.unchecked_into::<HtmlElement>())
    };
    // Show the current match, and say where it is among the others.
    let show = move || {
        found.with_value(|f| {
            let all = js_sys::Array::new();
            for range in &f.ranges {
                all.push(range);
            }
            let current = f.ranges.get(f.current);
            let marked = highlight(ALL, &all);
            let one = js_sys::Array::new();
            if let Some(range) = current {
                one.push(range);
            }
            highlight(CURRENT, &one);
            if !marked {
                select(current);
            }
            if let (Some(range), Some(scroller)) = (current, scroller()) {
                reveal(range, &scroller);
            }
            if !f.ranges.is_empty() {
                status.set(format!(
                    "{} of {}{}",
                    f.current + 1,
                    f.ranges.len(),
                    if f.ranges.len() == MATCHES_MAX {
                        "+"
                    } else {
                        ""
                    }
                ));
            }
        });
    };
    let clear = move || {
        found.set_value(Found::default());
        highlight(ALL, &js_sys::Array::new());
        highlight(CURRENT, &js_sys::Array::new());
    };
    // Search again, keeping near the match shown before, if `keep`.
    let search = move |keep: bool| {
        let Some(root) = content.get_untracked() else {
            return;
        };
        let query = Query {
            text: text.get_untracked(),
            regex: regex.get_untracked(),
            match_case: match_case.get_untracked(),
        };
        if query.text.is_empty() {
            clear();
            status.set(String::new());
            invalid.set(false);
            return;
        }
        let pattern = match query.pattern() {
            Ok(pattern) => pattern,
            Err(e) => {
                clear();
                invalid.set(true);
                status.set(e);
                return;
            }
        };
        invalid.set(false);
        let ranges = find(&root, &pattern);
        let previous = found.with_value(|f| f.current);
        if ranges.is_empty() {
            clear();
            status.set("No matches".into());
            return;
        }
        let current = if keep {
            previous.min(ranges.len() - 1)
        } else {
            0
        };
        found.set_value(Found { ranges, current });
        show();
    };
    let step = move |by: isize| {
        let moved = found.try_update_value(|f| {
            if f.ranges.is_empty() {
                return false;
            }
            let len = f.ranges.len() as isize;
            f.current = (f.current as isize + by).rem_euclid(len) as usize;
            true
        });
        if moved == Some(true) {
            show();
        }
    };
    let focus = move || {
        if let Some(input) = field.get_untracked() {
            let _ = input.focus();
            input.select();
        }
    };
    let start = move || {
        open.set(true);
        request_animation_frame(focus);
        search(true);
    };
    let close = move || {
        open.set(false);
        clear();
        select(None);
    };
    // The query changes: search again from the first match.
    Effect::new(move || {
        text.track();
        regex.track();
        match_case.track();
        if open.get_untracked() {
            search(false);
        }
    });
    // The preview's text changes, as when its code is highlighted: the ranges are stale.
    let changed = StoredValue::new_local(Some(Closure::<dyn FnMut()>::new(move || {
        if open.get_untracked() {
            search(true);
        }
    })));
    let unwatch = StoredValue::new_local(None::<js_sys::Function>);
    Effect::new(move || {
        if let Some(root) = content.get() {
            let stop = changed.with_value(|changed| {
                changed
                    .as_ref()
                    .map(|changed| watch(&root, changed.as_ref().unchecked_ref()))
            });
            unwatch.set_value(stop);
        }
    });
    // The viewer bar's button, shown while this preview is.
    let button = document().get_element_by_id("viewer-search");
    let opener = Closure::<dyn FnMut()>::new(start);
    if let Some(button) = &button {
        let _ = button.remove_attribute("hidden");
        let _ = button.add_event_listener_with_callback("click", opener.as_ref().unchecked_ref());
    }
    let opener = StoredValue::new_local(Some(opener));
    let keys = window_event_listener(ev::keydown, move |event| {
        let viewer = document()
            .query_selector("dialog#viewer[open]")
            .ok()
            .flatten()
            .is_some();
        let other = document()
            .query_selector("dialog#auth[open], dialog#editor[open]")
            .ok()
            .flatten()
            .is_some();
        if !viewer || other {
            return;
        }
        if (event.ctrl_key() || event.meta_key()) && event.key().eq_ignore_ascii_case("f") {
            event.prevent_default();
            start();
        } else if event.key() == "F3" && open.get_untracked() {
            event.prevent_default();
            step(if event.shift_key() { -1 } else { 1 });
        }
    });
    on_cleanup(move || {
        keys.remove();
        clear();
        if let Some(Some(stop)) = unwatch.try_get_value() {
            let _ = stop.call0(&JsValue::NULL);
        }
        if let Some(button) = &button {
            let _ = button.set_attribute("hidden", "");
            opener.try_with_value(|opener| {
                if let Some(opener) = opener {
                    let _ = button.remove_event_listener_with_callback(
                        "click",
                        opener.as_ref().unchecked_ref(),
                    );
                }
            });
        }
        let _ = opener.try_update_value(Option::take);
        let _ = changed.try_update_value(Option::take);
    });
    view! {
        <div class="preview-search" role="search" hidden=move || !open.get()>
            <input
                node_ref=field
                type="search"
                aria-label="Search the preview"
                placeholder=move || if regex.get() { "Regular expression" } else { "Text" }
                spellcheck="false"
                autocomplete="off"
                aria-invalid=move || invalid.get().to_string()
                prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
                on:keydown=move |e| match e.key().as_str() {
                    "Enter" => {
                        e.prevent_default();
                        step(if e.shift_key() { -1 } else { 1 });
                    }
                    "Escape" => {
                        // Close the search, not the viewer.
                        e.prevent_default();
                        e.stop_propagation();
                        close();
                    }
                    _ => {}
                }
            />
            <button
                type="button"
                class="search-toggle"
                title="Regular expression"
                aria-label="Regular expression"
                aria-pressed=move || regex.get().to_string()
                on:click=move |_| regex.update(|r| *r = !*r)
            >
                ".*"
            </button>
            <button
                type="button"
                class="search-toggle"
                title="Match case"
                aria-label="Match case"
                aria-pressed=move || match_case.get().to_string()
                on:click=move |_| match_case.update(|c| *c = !*c)
            >
                "Aa"
            </button>
            <span class="search-status" role="status" class:search-error=move || invalid.get()>
                {move || status.get()}
            </span>
            <button type="button" title="Previous match · Shift+Enter" aria-label="Previous match" on:click=move |_| step(-1)>"↑"</button>
            <button type="button" title="Next match · Enter" aria-label="Next match" on:click=move |_| step(1)>"↓"</button>
            <button type="button" title="Close search · Esc" aria-label="Close search" on:click=move |_| close()>"×"</button>
        </div>
        <div class="searchable" node_ref=content>
            {children()}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str, regex: bool, match_case: bool) -> Regex {
        Query {
            text: text.into(),
            regex,
            match_case,
        }
        .pattern()
        .unwrap()
    }
    fn pieces(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn matches_map_to_text_nodes_in_utf16_units() {
        // "fn" in one node, "main" across two, after a non-ASCII prefix.
        let text = pieces(&["γ fn ma", "in() {}\n", "fn x"]);
        let found = locate(&text, &query(r"fn \w+", true, false), 100);
        assert_eq!(found, [((0, 2), (1, 2)), ((2, 0), (2, 4))]);
        // An emoji is two UTF-16 units.
        let found = locate(&pieces(&["😀ab"]), &query("b", false, false), 100);
        assert_eq!(found, [((0, 3), (0, 4))]);
        // A match ending where a node ends stays in it; one starting there starts in the next.
        let found = locate(&pieces(&["ab", "cd"]), &query("ab|cd", true, false), 100);
        assert_eq!(found, [((0, 0), (0, 2)), ((1, 0), (1, 2))]);
    }

    #[test]
    fn queries_are_regular_expressions_or_text_with_line_anchors() {
        let text = pieces(&["Alpha\nbeta alpha\n"]);
        assert_eq!(locate(&text, &query("^\\w+", true, false), 100).len(), 2);
        assert_eq!(locate(&text, &query("alpha", true, true), 100).len(), 1);
        assert_eq!(locate(&text, &query("alpha", true, false), 100).len(), 2);
        assert_eq!(
            locate(&pieces(&["a.b axb"]), &query("a.b", false, false), 100).len(),
            1
        );
        // Empty matches are not matches; the limit holds.
        assert!(locate(&text, &query("x*", true, false), 100).is_empty());
        assert_eq!(
            locate(&pieces(&["aaaa"]), &query("a", true, false), 3).len(),
            3
        );
        let error = Query {
            text: "(".into(),
            regex: true,
            match_case: false,
        }
        .pattern()
        .unwrap_err();
        assert!(error.contains("unclosed"), "{error}");
        assert!(
            Query {
                text: "(".into(),
                regex: false,
                match_case: false
            }
            .pattern()
            .is_ok()
        );
    }
}
