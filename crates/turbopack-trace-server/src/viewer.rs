use std::{
    cmp::{max, Reverse},
    collections::{HashMap, HashSet},
};

use either::Either;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::{
    server::ViewRect,
    store::{SpanGraphEventRef, SpanGraphRef, SpanId, SpanRef, Store},
    u64_empty_string,
};

const EXTRA_WIDTH_PERCENTAGE: u64 = 50;
const EXTRA_HEIGHT: u64 = 5;

#[derive(Default)]
pub struct Viewer {
    span_options: HashMap<SpanId, SpanOptions>,
}

#[derive(Clone, Copy, Debug)]
pub enum ViewMode {
    RawSpans { sorted: bool },
    Aggregated { sorted: bool },
}

#[derive(Default)]
struct SpanOptions {
    view_mode: Option<(ViewMode, bool)>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ViewLineUpdate {
    y: u64,
    spans: Vec<ViewSpan>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ViewSpan {
    #[serde(with = "u64_empty_string")]
    id: u64,
    #[serde(rename = "x")]
    start: u64,
    #[serde(rename = "w")]
    width: u64,
    #[serde(rename = "cat")]
    category: String,
    #[serde(rename = "t")]
    text: String,
    #[serde(rename = "c")]
    count: u64,
    #[serde(rename = "k")]
    kind: u8,
}

#[derive(Debug)]
enum QueueItem<'a> {
    Span(SpanRef<'a>),
    SpanGraph(SpanGraphRef<'a>),
}

impl<'a> QueueItem<'a> {
    fn corrected_total_time(&self) -> u64 {
        match self {
            QueueItem::Span(span) => span.corrected_total_time(),
            QueueItem::SpanGraph(span_graph) => span_graph.corrected_total_time(),
        }
    }

    fn max_depth(&self) -> u32 {
        match self {
            QueueItem::Span(span) => span.max_depth(),
            QueueItem::SpanGraph(span_graph) => span_graph.max_depth(),
        }
    }
}

#[derive(Debug)]
struct QueueItemWithState<'a> {
    item: QueueItem<'a>,
    line_index: usize,
    start: u64,
    placeholder: bool,
    view_mode: ViewMode,
    filtered: bool,
}

struct ChildItem<'a> {
    item: QueueItemWithState<'a>,
    depth: u32,
    pixel_range: (u64, u64),
}

impl Viewer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_view_mode(&mut self, id: SpanId, view_mode: Option<(ViewMode, bool)>) {
        self.span_options.entry(id).or_default().view_mode = view_mode;
    }

    pub fn compute_update(&mut self, store: &Store, view_rect: &ViewRect) -> Vec<ViewLineUpdate> {
        let mut highlighted_spans: HashSet<SpanId> = HashSet::new();
        let search_mode = !view_rect.query.is_empty();

        let mut queue = Vec::new();

        let mut root_spans = store.root_spans().collect::<Vec<_>>();
        root_spans.sort_by_key(|span| span.start());
        let mut children = Vec::new();
        let mut current = 0;
        for span in root_spans {
            // Move current to start if needed.
            current = max(current, span.start());
            if add_child_item(
                &mut children,
                &mut current,
                view_rect,
                0,
                if span.is_complete() && view_rect.query.is_empty() {
                    ViewMode::Aggregated { sorted: false }
                } else {
                    ViewMode::RawSpans { sorted: false }
                },
                QueueItem::Span(span),
                false,
            ) && search_mode
            {
                let mut has_results = false;
                for mut result in span.search(&view_rect.query) {
                    has_results = true;
                    highlighted_spans.insert(result.id());
                    while let Some(parent) = result.parent() {
                        result = parent;
                        if !highlighted_spans.insert(result.id()) {
                            break;
                        }
                    }
                }
                if !has_results {
                    children.last_mut().unwrap().item.filtered = true;
                }
            }
        }
        enqueue_children(children, &mut queue);

        let mut lines: Vec<Vec<LineEntry<'_>>> = vec![];

        while let Some(QueueItemWithState {
            item: span,
            line_index,
            start,
            placeholder,
            view_mode,
            filtered,
        }) = queue.pop()
        {
            let line = get_line(&mut lines, line_index);
            let width = span.corrected_total_time();

            // compute children
            let mut children = Vec::new();
            let mut current = start;
            match &span {
                QueueItem::Span(span) => {
                    let (selected_view_mode, inherit) = self
                        .span_options
                        .get(&span.id())
                        .and_then(|o| o.view_mode)
                        .unwrap_or((view_mode, false));

                    let (show_children, sorted) = match selected_view_mode {
                        ViewMode::RawSpans { sorted } => (true, sorted),
                        ViewMode::Aggregated { sorted } => (false, sorted),
                    };
                    let view_mode = if inherit {
                        selected_view_mode
                    } else {
                        view_mode
                    };
                    if show_children {
                        let spans = if sorted {
                            Either::Left(span.children().sorted_by_cached_key(|child| {
                                Reverse(child.corrected_total_time())
                            }))
                        } else {
                            Either::Right(span.children())
                        };
                        for child in spans {
                            let filtered = search_mode && !highlighted_spans.contains(&child.id());
                            add_child_item(
                                &mut children,
                                &mut current,
                                view_rect,
                                line_index + 1,
                                view_mode,
                                QueueItem::Span(child),
                                filtered,
                            );
                        }
                    } else {
                        let events = if sorted {
                            Either::Left(span.graph().sorted_by_cached_key(|child| {
                                Reverse(child.corrected_total_time())
                            }))
                        } else {
                            Either::Right(span.graph())
                        };
                        for event in events {
                            match event {
                                SpanGraphEventRef::SelfTime { duration: _ } => {}
                                SpanGraphEventRef::Child { graph } => {
                                    add_child_item(
                                        &mut children,
                                        &mut current,
                                        view_rect,
                                        line_index + 1,
                                        view_mode,
                                        QueueItem::SpanGraph(graph),
                                        false,
                                    );
                                }
                            }
                        }
                    }
                }
                QueueItem::SpanGraph(span_graph) => {
                    let (selected_view_mode, inherit) = self
                        .span_options
                        .get(&span_graph.id())
                        .and_then(|o| o.view_mode)
                        .unwrap_or((view_mode, false));

                    let (show_spans, sorted) = match selected_view_mode {
                        ViewMode::RawSpans { sorted } => (true, sorted),
                        ViewMode::Aggregated { sorted } => (false, sorted),
                    };
                    let view_mode = if inherit {
                        selected_view_mode
                    } else {
                        view_mode
                    };
                    if show_spans && span_graph.count() > 1 {
                        let spans = if sorted {
                            Either::Left(span_graph.root_spans().sorted_by_cached_key(|child| {
                                Reverse(child.corrected_total_time())
                            }))
                        } else {
                            Either::Right(span_graph.root_spans())
                        };
                        for child in spans {
                            let filtered = search_mode && !highlighted_spans.contains(&child.id());
                            add_child_item(
                                &mut children,
                                &mut current,
                                view_rect,
                                line_index + 1,
                                view_mode,
                                QueueItem::Span(child),
                                filtered,
                            );
                        }
                    } else {
                        let events = if sorted {
                            Either::Left(span_graph.children().sorted_by_cached_key(|child| {
                                Reverse(child.corrected_total_time())
                            }))
                        } else {
                            Either::Right(span_graph.children())
                        };
                        for child in events {
                            add_child_item(
                                &mut children,
                                &mut current,
                                view_rect,
                                line_index + 1,
                                view_mode,
                                QueueItem::SpanGraph(child),
                                false,
                            );
                        }
                    }
                }
            }

            // When span size is smaller than a pixel, we only show the deepest child.
            if placeholder {
                let child = children
                    .into_iter()
                    .max_by_key(|ChildItem { item, depth, .. }| (!item.filtered, *depth));
                if let Some(ChildItem {
                    item: mut entry, ..
                }) = child
                {
                    entry.placeholder = true;
                    queue.push(entry);
                }

                // add span to line
                line.push(LineEntry {
                    start,
                    width,
                    ty: LineEntryType::Placeholder(filtered),
                });
            } else {
                // add children to queue
                enqueue_children(children, &mut queue);

                // add span to line
                line.push(LineEntry {
                    start,
                    width,
                    ty: match span {
                        QueueItem::Span(span) => LineEntryType::Span(span, filtered),
                        QueueItem::SpanGraph(span_graph) => {
                            LineEntryType::SpanGraph(span_graph, filtered)
                        }
                    },
                });
            }
        }

        lines
            .into_iter()
            .enumerate()
            .map(|(y, line)| ViewLineUpdate {
                y: y as u64,
                spans: line
                    .into_iter()
                    .map(|entry| match entry.ty {
                        LineEntryType::Placeholder(filtered) => ViewSpan {
                            id: 0,
                            start: entry.start,
                            width: entry.width,
                            category: String::new(),
                            text: String::new(),
                            count: 1,
                            kind: if filtered { 3 } else { 1 },
                        },
                        LineEntryType::Span(span, filtered) => {
                            let (category, text) = span.nice_name();
                            ViewSpan {
                                id: span.id().get() as u64,
                                start: entry.start,
                                width: entry.width,
                                category: category.to_string(),
                                text: text.to_string(),
                                count: 1,
                                kind: if filtered { 2 } else { 0 },
                            }
                        }
                        LineEntryType::SpanGraph(graph, filtered) => {
                            let (category, text) = graph.nice_name();
                            ViewSpan {
                                id: graph.id().get() as u64,
                                start: entry.start,
                                width: entry.width,
                                category: category.to_string(),
                                text: text.to_string(),
                                count: graph.count() as u64,
                                kind: if filtered { 2 } else { 0 },
                            }
                        }
                    })
                    .collect(),
            })
            .collect()
    }
}

fn add_child_item<'a>(
    children: &mut Vec<ChildItem<'a>>,
    current: &mut u64,
    view_rect: &ViewRect,
    line_index: usize,
    view_mode: ViewMode,
    child: QueueItem<'a>,
    filtered: bool,
) -> bool {
    let child_width = child.corrected_total_time();
    let max_depth = child.max_depth();
    let pixel1 = *current * view_rect.horizontal_pixels / view_rect.width;
    let pixel2 = ((*current + child_width) * view_rect.horizontal_pixels + view_rect.width - 1)
        / view_rect.width;
    let start = *current;
    *current += child_width;

    // filter by view rect (vertical)
    if line_index > (view_rect.y + view_rect.height + EXTRA_HEIGHT) as usize {
        return false;
    }

    if line_index > 0 {
        // filter by view rect (horizontal)
        if start > view_rect.x + view_rect.width * (100 + EXTRA_WIDTH_PERCENTAGE) / 100 {
            return false;
        }
        if *current
            < view_rect
                .x
                .saturating_sub(view_rect.width * EXTRA_WIDTH_PERCENTAGE / 100)
        {
            return false;
        }
    }

    children.push(ChildItem {
        item: QueueItemWithState {
            item: child,
            line_index,
            start,
            placeholder: false,
            view_mode,
            filtered,
        },
        depth: max_depth,
        pixel_range: (pixel1, pixel2),
    });

    true
}

const MIN_VISIBLE_PIXEL_SIZE: u64 = 3;

fn enqueue_children<'a>(mut children: Vec<ChildItem<'a>>, queue: &mut Vec<QueueItemWithState<'a>>) {
    children.reverse();
    let mut last_pixel = u64::MAX;
    let mut last_max_depth = 0;
    for ChildItem {
        item: mut entry,
        depth: max_depth,
        pixel_range: (pixel1, pixel2),
    } in children
    {
        if last_pixel <= pixel1 + MIN_VISIBLE_PIXEL_SIZE {
            if last_max_depth < max_depth {
                queue.pop();
                entry.placeholder = true;
            } else {
                if let Some(entry) = queue.last_mut() {
                    entry.placeholder = true;
                }
                continue;
            }
        };
        queue.push(entry);
        last_max_depth = max_depth;
        last_pixel = pixel2;
    }
}

fn get_line<T: Default>(lines: &mut Vec<T>, i: usize) -> &mut T {
    if i >= lines.len() {
        lines.resize_with(i + 1, || Default::default());
    }
    &mut lines[i]
}

struct LineEntry<'a> {
    start: u64,
    width: u64,
    ty: LineEntryType<'a>,
}

enum LineEntryType<'a> {
    Placeholder(bool),
    Span(SpanRef<'a>, bool),
    SpanGraph(SpanGraphRef<'a>, bool),
}
