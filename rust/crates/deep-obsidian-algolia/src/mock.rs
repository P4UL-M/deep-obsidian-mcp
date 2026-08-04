//! In-process mock of the Algolia REST endpoints the client uses.
//!
//! Implements enough engine behaviour for tests and the demo: token/prefix
//! matching over searchable attributes, `filters` (AND / NOT / parenthesized
//! OR, quoted values, dotted paths, array membership), facet counts,
//! `distinct` by the configured attribute, browse with cursors, batch writes,
//! delete-by-query, and get-objects. It is a fidelity aid, not a search
//! engine: ranking is a simple token-overlap score with `updatedAtMs`
//! tie-break.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct MockIndex {
    settings: Value,
    objects: BTreeMap<String, Value>,
}

#[derive(Clone, Default)]
pub struct MockAlgolia {
    indexes: Arc<Mutex<HashMap<String, MockIndex>>>,
    /// objectIDs this mock answers with Algolia's 403 `objectID not allowed`.
    ///
    /// Mimics a SECURED key whose `filters` restriction excludes an object: the real
    /// engine does NOT answer "not found" there, it answers 403 with that message, and
    /// a caller that surfaces the difference lets a scoped participant enumerate paths
    /// outside their scope. Without this the anti-enumeration mapping could only be
    /// verified against a live account.
    forbidden_object_ids: Arc<Vec<String>>,
}

impl MockAlgolia {
    /// A mock that refuses `object_ids` with the secured-key 403.
    pub fn with_forbidden_object_ids(object_ids: Vec<String>) -> Self {
        Self {
            indexes: Arc::new(Mutex::new(HashMap::new())),
            forbidden_object_ids: Arc::new(object_ids),
        }
    }
}

/// Binds to an ephemeral loopback port and serves the mock; returns the base
/// URL to pass as the client's `base_url` override.
pub async fn spawn_mock() -> (String, tokio::task::JoinHandle<()>) {
    spawn_mock_with(MockAlgolia::default()).await
}

/// [`spawn_mock`] over a pre-configured mock, e.g. one that refuses some objectIDs.
pub async fn spawn_mock_with(state: MockAlgolia) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router(state))
            .await
            .expect("mock server");
    });
    (format!("http://{addr}"), handle)
}

/// Serves the mock on a fixed port (demo usage).
pub async fn serve_on(port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("mock-algolia listening on http://127.0.0.1:{port}");
    axum::serve(listener, router(MockAlgolia::default()))
        .await
        .map_err(std::io::Error::other)
}

pub fn router(state: MockAlgolia) -> Router {
    Router::new()
        .route("/1/indexes/{index}/batch", post(handle_batch))
        .route("/1/indexes/{index}/query", post(handle_query))
        .route("/1/indexes/{index}/browse", post(handle_browse))
        .route("/1/indexes/{index}/deleteByQuery", post(handle_delete_by_query))
        .route(
            "/1/indexes/{index}/settings",
            get(handle_get_settings).put(handle_set_settings),
        )
        .route("/1/indexes/{index}/objects", post(handle_get_objects))
        .route("/1/indexes/{index}/task/{task}", get(handle_task_status))
        .route(
            "/1/indexes/{index}/facets/{facet}/query",
            post(handle_facet_query),
        )
        .with_state(state)
}

// --- params parsing -------------------------------------------------------

fn parse_params(params: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in params.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let decoded = urlencoding::decode(value).unwrap_or_default().into_owned();
            map.insert(key.to_string(), decoded);
        }
    }
    map
}

fn body_params(body: &Value) -> HashMap<String, String> {
    body.get("params")
        .and_then(Value::as_str)
        .map(parse_params)
        .unwrap_or_default()
}

// --- filter evaluation -----------------------------------------------------

/// Resolves a dotted attribute path (`folders.lvl0`) inside a record.
fn lookup<'a>(record: &'a Value, attr: &str) -> Option<&'a Value> {
    let mut current = record;
    for segment in attr.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn value_matches(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(text) => text == expected,
        Value::Bool(flag) => expected.eq_ignore_ascii_case(if *flag { "true" } else { "false" }),
        Value::Number(number) => number.to_string() == expected,
        Value::Array(items) => items.iter().any(|item| value_matches(item, expected)),
        _ => false,
    }
}

/// Signals a filter string Algolia would reject with a 400, so the mock does
/// not accept queries the real engine refuses.
fn invalid_filter(filters: &str) -> Option<String> {
    for clause in split_top_level(filters, " AND ") {
        let clause = clause.trim().trim_start_matches("NOT ").trim();
        if let Some((attr, raw_value)) = clause.split_once(':') {
            if raw_value.trim().trim_matches('"').is_empty() {
                return Some(format!(
                    "filters: Not allowed empty string at col {}",
                    attr.len() + 1
                ));
            }
        }
    }
    None
}

fn eval_atom(record: &Value, atom: &str) -> bool {
    let atom = atom.trim();
    if let Some(rest) = atom.strip_prefix("NOT ") {
        return !eval_atom(record, rest);
    }
    let Some((attr, raw_value)) = atom.split_once(':') else {
        return true; // unparseable clause: permissive
    };
    let expected = raw_value.trim().trim_matches('"');
    lookup(record, attr.trim())
        .map(|value| value_matches(value, expected))
        .unwrap_or(false)
}

/// Evaluates ` AND `-joined clauses; each clause is an atom, `NOT atom`, or a
/// parenthesized ` OR `-joined group of atoms.
fn eval_filters(record: &Value, filters: &str) -> bool {
    let filters = filters.trim();
    if filters.is_empty() {
        return true;
    }
    split_top_level(filters, " AND ").iter().all(|clause| {
        let clause = clause.trim();
        if clause.starts_with('(') && clause.ends_with(')') {
            let inner = &clause[1..clause.len() - 1];
            inner
                .split(" OR ")
                .any(|atom| eval_atom(record, atom))
        } else {
            eval_atom(record, clause)
        }
    })
}

/// Splits on a separator while respecting parentheses and double quotes.
fn split_top_level(text: &str, separator: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_quotes = false;
    let mut current = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && text[i..].starts_with(separator) {
                parts.push(current.clone());
                current.clear();
                i += separator.len();
                continue;
            }
        }
        current.push(ch);
        i += 1;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

// --- query matching --------------------------------------------------------

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn searchable_text(record: &Value, settings: &Value) -> String {
    let attrs: Vec<String> = settings
        .get("searchableAttributes")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(|attr| {
                    attr.trim_start_matches("unordered(")
                        .trim_end_matches(')')
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_else(|| vec!["title".into(), "headings".into(), "text".into(), "path".into()]);
    let mut parts = Vec::new();
    for attr in attrs {
        if let Some(value) = lookup(record, &attr) {
            match value {
                Value::String(text) => parts.push(text.clone()),
                Value::Array(items) => {
                    parts.extend(items.iter().filter_map(Value::as_str).map(str::to_string))
                }
                _ => {}
            }
        }
    }
    parts.join("\n")
}

/// Token-overlap score: every query token must match some record token (exact
/// or prefix); exact matches score higher. Returns `None` when a token has no
/// match at all.
fn match_score(record_tokens: &[String], query_tokens: &[String]) -> Option<usize> {
    let mut score = 0usize;
    for token in query_tokens {
        if record_tokens.iter().any(|candidate| candidate == token) {
            score += 2;
        } else if record_tokens
            .iter()
            .any(|candidate| candidate.starts_with(token.as_str()))
        {
            score += 1;
        } else {
            return None;
        }
    }
    Some(score)
}

fn updated_at(record: &Value) -> u64 {
    record
        .get("updatedAtMs")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

struct RankedHits {
    hits: Vec<Value>,
    facets: Option<Value>,
}

fn run_search(index: &MockIndex, params: &HashMap<String, String>) -> RankedHits {
    let query = params.get("query").cloned().unwrap_or_default();
    let filters = params.get("filters").cloned().unwrap_or_default();
    let query_tokens = tokenize(&query);

    let restrict: Option<Vec<String>> = params
        .get("restrictSearchableAttributes")
        .and_then(|raw| serde_json::from_str(raw).ok());

    let mut scored: Vec<(usize, Value)> = Vec::new();
    for record in index.objects.values() {
        if !eval_filters(record, &filters) {
            continue;
        }
        let haystack = match &restrict {
            Some(attrs) => attrs
                .iter()
                .filter_map(|attr| lookup(record, attr))
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            None => searchable_text(record, &index.settings),
        };
        let record_tokens = tokenize(&haystack);
        match match_score(&record_tokens, &query_tokens) {
            Some(score) => scored.push((score, record.clone())),
            None => continue,
        }
    }
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| updated_at(&right.1).cmp(&updated_at(&left.1)))
    });

    // Facet counts over the filtered+matched set, pre-distinct.
    let facets_requested: Vec<String> = params
        .get("facets")
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let facets = if facets_requested.is_empty() {
        None
    } else {
        let mut counts: Map<String, Value> = Map::new();
        for facet in &facets_requested {
            let mut values: BTreeMap<String, usize> = BTreeMap::new();
            for (_, record) in &scored {
                if let Some(value) = lookup(record, facet) {
                    match value {
                        Value::String(text) => *values.entry(text.clone()).or_default() += 1,
                        Value::Array(items) => {
                            for item in items.iter().filter_map(Value::as_str) {
                                *values.entry(item.to_string()).or_default() += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            counts.insert(
                facet.clone(),
                Value::Object(values.into_iter().map(|(k, v)| (k, json!(v))).collect()),
            );
        }
        Some(Value::Object(counts))
    };

    // Distinct by the configured attribute unless the request disables it.
    let distinct_enabled = params
        .get("distinct")
        .map(|value| value != "false" && value != "0")
        .unwrap_or_else(|| {
            index
                .settings
                .get("distinct")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= 1
        });
    let distinct_attr = index
        .settings
        .get("attributeForDistinct")
        .and_then(Value::as_str)
        .map(str::to_string);
    let hits: Vec<Value> = if let Some(attr) = distinct_attr.filter(|_| distinct_enabled) {
        let mut seen: Vec<String> = Vec::new();
        scored
            .into_iter()
            .filter(|(_, record)| {
                let key = lookup(record, &attr)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if key.is_empty() || !seen.contains(&key) {
                    seen.push(key);
                    true
                } else {
                    false
                }
            })
            .map(|(_, record)| record)
            .collect()
    } else {
        scored.into_iter().map(|(_, record)| record).collect()
    };

    RankedHits { hits, facets }
}

// --- handlers ---------------------------------------------------------------

type SharedState = State<MockAlgolia>;

/// WRITE accessor: creates the index on first write, mirroring Algolia (an
/// index springs into existence when you first write to it).
fn with_index<T>(
    state: &MockAlgolia,
    index: &str,
    action: impl FnOnce(&mut MockIndex) -> T,
) -> T {
    let mut indexes = state.indexes.lock().expect("mock lock");
    let entry = indexes.entry(index.to_string()).or_default();
    action(entry)
}

/// READ accessor: `None` when the index was never written to. Algolia answers
/// 404 `Index <name> does not exist` for reads against a missing index — the
/// permissive auto-create used to hide that whole bug class from the tests.
fn with_existing_index<T>(
    state: &MockAlgolia,
    index: &str,
    action: impl FnOnce(&MockIndex) -> T,
) -> Option<T> {
    let indexes = state.indexes.lock().expect("mock lock");
    indexes.get(index).map(action)
}

fn index_missing(index: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": format!("Index {index} does not exist") })),
    )
}

async fn handle_batch(
    State(state): SharedState,
    AxumPath(index): AxumPath<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let requests = body
        .get("requests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut object_ids = Vec::new();
    with_index(&state, &index, |mock_index| {
        for request in &requests {
            let action = request.get("action").and_then(Value::as_str).unwrap_or("");
            let Some(request_body) = request.get("body") else {
                continue;
            };
            let object_id = request_body
                .get("objectID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match action {
                "addObject" | "updateObject" | "partialUpdateObject" => {
                    mock_index
                        .objects
                        .insert(object_id.clone(), request_body.clone());
                }
                "deleteObject" => {
                    mock_index.objects.remove(&object_id);
                }
                _ => {}
            }
            object_ids.push(Value::String(object_id));
        }
    });
    (
        StatusCode::OK,
        Json(json!({ "taskID": 1, "objectIDs": object_ids })),
    )
}

async fn handle_query(
    State(state): SharedState,
    AxumPath(index): AxumPath<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let params = body_params(&body);
    if let Some(message) = params.get("filters").and_then(|f| invalid_filter(f)) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "message": message })));
    }
    let Some((hits, facets)) = with_existing_index(&state, &index, |mock_index| {
        let ranked = run_search(mock_index, &params);
        (ranked.hits, ranked.facets)
    }) else {
        return index_missing(&index);
    };
    let hits_per_page: usize = params
        .get("hitsPerPage")
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let page: usize = params
        .get("page")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let nb_hits = hits.len();
    let page_hits: Vec<Value> = hits
        .into_iter()
        .skip(page * hits_per_page)
        .take(hits_per_page)
        .collect();
    let mut response = json!({
        "hits": page_hits,
        "nbHits": nb_hits,
        "page": page,
        "hitsPerPage": hits_per_page,
    });
    if let Some(facets) = facets {
        response["facets"] = facets;
    }
    (StatusCode::OK, Json(response))
}

async fn handle_browse(
    State(state): SharedState,
    AxumPath(index): AxumPath<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let params = body_params(&body);
    let filters = params.get("filters").cloned().unwrap_or_default();
    if let Some(message) = invalid_filter(&filters) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "message": message })));
    }
    let offset: usize = body
        .get("cursor")
        .and_then(Value::as_str)
        .and_then(|cursor| cursor.parse().ok())
        .unwrap_or(0);
    let page_size: usize = params
        .get("hitsPerPage")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1000);
    let Some(all): Option<Vec<Value>> = with_existing_index(&state, &index, |mock_index| {
        mock_index
            .objects
            .values()
            .filter(|record| eval_filters(record, &filters))
            .cloned()
            .collect()
    }) else {
        return index_missing(&index);
    };
    let hits: Vec<Value> = all.iter().skip(offset).take(page_size).cloned().collect();
    let next = offset + hits.len();
    let mut response = json!({ "hits": hits, "nbHits": all.len() });
    if next < all.len() {
        response["cursor"] = json!(next.to_string());
    }
    (StatusCode::OK, Json(response))
}

async fn handle_delete_by_query(
    State(state): SharedState,
    AxumPath(index): AxumPath<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let params = body_params(&body);
    let filters = params.get("filters").cloned().unwrap_or_default();
    if let Some(message) = invalid_filter(&filters) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "message": message })));
    }
    let mut indexes = state.indexes.lock().expect("mock lock");
    let Some(mock_index) = indexes.get_mut(&index) else {
        drop(indexes);
        return index_missing(&index);
    };
    let removed = {
        let doomed: Vec<String> = mock_index
            .objects
            .iter()
            .filter(|(_, record)| eval_filters(record, &filters))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &doomed {
            mock_index.objects.remove(id);
        }
        doomed.len()
    };
    drop(indexes);
    (
        StatusCode::OK,
        Json(json!({ "taskID": 1, "deletedCount": removed })),
    )
}

/// Mock writes are synchronous, so a task is always already published. The
/// endpoint exists so the production wait-for-task path is exercised by tests.
async fn handle_task_status(
    AxumPath((_index, _task)): AxumPath<(String, String)>,
) -> Json<Value> {
    Json(json!({ "status": "published", "pendingTask": false }))
}

async fn handle_get_settings(
    State(state): SharedState,
    AxumPath(index): AxumPath<String>,
) -> Json<Value> {
    let settings = with_index(&state, &index, |mock_index| mock_index.settings.clone());
    Json(if settings.is_null() {
        json!({})
    } else {
        settings
    })
}

async fn handle_set_settings(
    State(state): SharedState,
    AxumPath(index): AxumPath<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    with_index(&state, &index, |mock_index| {
        mock_index.settings = body;
    });
    Json(json!({ "taskID": 1 }))
}

/// `POST /1/indexes/*/objects` — the wildcard arrives as the literal `*`
/// segment; requests name their index explicitly.
async fn handle_get_objects(
    State(state): SharedState,
    AxumPath(_index): AxumPath<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let requests = body
        .get("requests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // A secured key's restriction is evaluated per request, and the whole call fails —
    // not just the offending entry. Mirrored, because a caller that only checked
    // individual results would never see the 403 at all.
    if requests.iter().any(|request| {
        request
            .get("objectID")
            .and_then(Value::as_str)
            .is_some_and(|id| state.forbidden_object_ids.iter().any(|entry| entry == id))
    }) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "message": "Method not allowed with this API key (objectID not allowed)"
            })),
        );
    }
    let mut results = Vec::new();
    for request in requests {
        let index_name = request
            .get("indexName")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let object_id = request
            .get("objectID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let found = with_index(&state, index_name, |mock_index| {
            mock_index.objects.get(object_id).cloned()
        });
        results.push(found.unwrap_or(Value::Null));
    }
    (StatusCode::OK, Json(json!({ "results": results })))
}

async fn handle_facet_query(
    State(state): SharedState,
    AxumPath((index, facet)): AxumPath<(String, String)>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let params = body_params(&body);
    let facet_query = params.get("facetQuery").cloned().unwrap_or_default();
    let filters = params.get("filters").cloned().unwrap_or_default();
    let max_hits: usize = params
        .get("maxFacetHits")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    // Algolia 400s above 100 rather than clamping; mirror that so callers
    // cannot ship a request the real engine refuses.
    if max_hits > 100 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "message": format!(
                    "Value \"{max_hits}\" outside of the range for \"maxFacetHits\" parameter, expected integer between 1 and 100"
                )
            })),
        );
    }
    if let Some(message) = invalid_filter(&filters) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "message": message })));
    }
    let needle = facet_query.to_lowercase();
    let Some(counts): Option<BTreeMap<String, usize>> =
        with_existing_index(&state, &index, |mock_index| {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for record in mock_index.objects.values() {
            if !eval_filters(record, &filters) {
                continue;
            }
            if let Some(value) = lookup(record, &facet) {
                let candidates: Vec<String> = match value {
                    Value::String(text) => vec![text.clone()],
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    _ => Vec::new(),
                };
                for candidate in candidates {
                    if needle.is_empty() || candidate.to_lowercase().contains(&needle) {
                        *counts.entry(candidate).or_default() += 1;
                    }
                }
            }
        }
            counts
        })
    else {
        return index_missing(&index);
    };
    let mut hits: Vec<(String, usize)> = counts.into_iter().collect();
    hits.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let facet_hits: Vec<Value> = hits
        .into_iter()
        .take(max_hits)
        .map(|(value, count)| json!({ "value": value, "count": count, "highlighted": value }))
        .collect();
    (StatusCode::OK, Json(json!({ "facetHits": facet_hits })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, extra: Value) -> Value {
        let mut base = json!({ "objectID": id });
        if let (Some(base_map), Some(extra_map)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra_map {
                base_map.insert(key.clone(), value.clone());
            }
        }
        base
    }

    #[test]
    fn filters_support_and_not_or_and_arrays() {
        let note = record(
            "note:1",
            json!({
                "recordType": "note",
                "dir": "_Wiki/Decisions",
                "folders": { "lvl0": "_Wiki" },
                "links": ["A.md", "B.md"],
                "deleted": false
            }),
        );
        assert!(eval_filters(&note, "recordType:note"));
        assert!(eval_filters(&note, "recordType:note AND folders.lvl0:_Wiki"));
        assert!(eval_filters(&note, "links:\"A.md\""));
        assert!(eval_filters(&note, "NOT deleted:true"));
        assert!(eval_filters(&note, "(recordType:chunk OR recordType:note)"));
        assert!(!eval_filters(&note, "recordType:chunk"));
        assert!(!eval_filters(&note, "links:\"C.md\""));
    }

    #[test]
    fn split_top_level_respects_quotes_and_parens() {
        let parts = split_top_level(
            "a:\"x AND y\" AND (b:1 OR b:2) AND c:3",
            " AND ",
        );
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "a:\"x AND y\"");
        assert_eq!(parts[1], "(b:1 OR b:2)");
    }

    #[tokio::test]
    async fn end_to_end_batch_query_distinct_and_facets() {
        let (base_url, _handle) = spawn_mock().await;
        let client = crate::AlgoliaClient::new("TESTAPP", "test-key", Some(&base_url));

        client
            .set_settings("wiki", crate::records::main_index_settings())
            .await
            .expect("set settings");
        client
            .save_objects(
                "wiki",
                vec![
                    record("chunk:A@v1#0", json!({
                        "recordType": "chunk", "path": "A.md", "noteId": "A.md",
                        "title": "Alpha decisions", "text": "retrieval architecture stays agnostic",
                        "folders": {"lvl0": "_Wiki"}, "updatedAtMs": 2
                    })),
                    record("chunk:A@v1#1", json!({
                        "recordType": "chunk", "path": "A.md", "noteId": "A.md",
                        "title": "Alpha decisions", "text": "architecture notes continued",
                        "folders": {"lvl0": "_Wiki"}, "updatedAtMs": 2
                    })),
                    record("chunk:B@v1#0", json!({
                        "recordType": "chunk", "path": "B.md", "noteId": "B.md",
                        "title": "Beta", "text": "unrelated content about packaging",
                        "folders": {"lvl0": "_Agent"}, "updatedAtMs": 1
                    })),
                ],
            )
            .await
            .expect("save objects");

        // Distinct collapses the two A.md chunks into one hit.
        let response = client
            .search(
                "wiki",
                &crate::SearchRequest {
                    query: "architecture".to_string(),
                    facets: vec!["folders.lvl0".to_string()],
                    ..Default::default()
                },
            )
            .await
            .expect("search");
        assert_eq!(response.hits.len(), 1);
        assert_eq!(
            response.hits[0].get("path").and_then(Value::as_str),
            Some("A.md")
        );
        let facets = response.facets.expect("facets");
        assert_eq!(
            facets.pointer("/folders.lvl0/_Wiki").and_then(Value::as_u64),
            Some(2)
        );

        // distinct=false returns both chunks.
        let response = client
            .search(
                "wiki",
                &crate::SearchRequest {
                    query: String::new(),
                    filters: Some("noteId:\"A.md\"".to_string()),
                    distinct: Some(false),
                    hits_per_page: Some(10),
                    ..Default::default()
                },
            )
            .await
            .expect("search all chunks");
        assert_eq!(response.hits.len(), 2);

        // deleteByQuery removes only the filtered records.
        client
            .delete_by_query("wiki", "noteId:\"A.md\"")
            .await
            .expect("delete by query");
        let remaining = client.browse_all("wiki", None).await.expect("browse");
        assert_eq!(remaining.len(), 1);

        // getObjects returns null for missing ids.
        let fetched = client
            .get_objects("wiki", &["chunk:B@v1#0".to_string(), "chunk:A@v1#0".to_string()])
            .await
            .expect("get objects");
        assert!(fetched[0].is_some());
        assert!(fetched[1].is_none());
    }

    #[tokio::test]
    async fn facet_value_search_counts_and_filters() {
        let (base_url, _handle) = spawn_mock().await;
        let client = crate::AlgoliaClient::new("TESTAPP", "test-key", Some(&base_url));
        client
            .save_objects(
                "wiki",
                vec![
                    record("n1", json!({"recordType": "note", "dir": "_Wiki/Decisions", "folders": {"lvl0": "_Wiki", "lvl1": "_Wiki/Decisions"}})),
                    record("n2", json!({"recordType": "note", "dir": "_Wiki/Syntheses", "folders": {"lvl0": "_Wiki", "lvl1": "_Wiki/Syntheses"}})),
                    record("c1", json!({"recordType": "chunk", "dir": "_Wiki/Decisions", "folders": {"lvl0": "_Wiki", "lvl1": "_Wiki/Decisions"}})),
                ],
            )
            .await
            .expect("save");
        let hits = client
            .search_facet_values("wiki", "folders.lvl1", "", Some("recordType:note"), 10)
            .await
            .expect("facet search");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.count == 1));
    }
}
