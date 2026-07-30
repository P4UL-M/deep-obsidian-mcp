//! Async Algolia REST client.
//!
//! Speaks to `https://{appId}.algolia.net` by default; `base_url` can be
//! overridden to point at the in-process mock (tests, demo) or a proxy.
//! Single-host, no retry fan-out to the `-1..-3.algolianet.com` fallbacks —
//! acceptable for v1, noted in the design doc.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;

#[derive(Debug, thiserror::Error)]
pub enum AlgoliaError {
    #[error("algolia http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("algolia api error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("algolia unexpected response: {0}")]
    InvalidResponse(String),
}

impl AlgoliaError {
    /// True for Algolia's 404 `Index <name> does not exist`. An index springs
    /// into existence on its first WRITE, so every read against a never-written
    /// index answers this — callers that treat "no index" as "no records" check
    /// it instead of failing.
    pub fn is_index_not_found(&self) -> bool {
        matches!(
            self,
            AlgoliaError::Api { status: 404, message } if message.contains("does not exist")
        )
    }
}

pub type Result<T> = std::result::Result<T, AlgoliaError>;

#[derive(Debug, Clone)]
pub struct AlgoliaClient {
    http: reqwest::Client,
    app_id: String,
    api_key: String,
    base_url: String,
}

/// A search request, serialized into Algolia's url-encoded `params` form (the
/// portable representation the REST API accepts everywhere).
#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: String,
    pub filters: Option<String>,
    pub hits_per_page: Option<usize>,
    pub page: Option<usize>,
    pub facets: Vec<String>,
    pub max_values_per_facet: Option<usize>,
    pub restrict_searchable_attributes: Vec<String>,
    /// `Some(false)` disables the index-level distinct for this query (needed
    /// when fetching all chunks of one note); `None` keeps the index default.
    pub distinct: Option<bool>,
    pub attributes_to_retrieve: Vec<String>,
}

impl SearchRequest {
    pub fn to_params(&self) -> String {
        let mut params: Vec<String> = Vec::new();
        params.push(format!("query={}", urlencoding::encode(&self.query)));
        if let Some(filters) = &self.filters {
            params.push(format!("filters={}", urlencoding::encode(filters)));
        }
        if let Some(hits) = self.hits_per_page {
            params.push(format!("hitsPerPage={hits}"));
        }
        if let Some(page) = self.page {
            params.push(format!("page={page}"));
        }
        if !self.facets.is_empty() {
            let encoded = serde_json::to_string(&self.facets).unwrap_or_default();
            params.push(format!("facets={}", urlencoding::encode(&encoded)));
        }
        if let Some(max) = self.max_values_per_facet {
            params.push(format!("maxValuesPerFacet={max}"));
        }
        if !self.restrict_searchable_attributes.is_empty() {
            let encoded =
                serde_json::to_string(&self.restrict_searchable_attributes).unwrap_or_default();
            params.push(format!(
                "restrictSearchableAttributes={}",
                urlencoding::encode(&encoded)
            ));
        }
        if let Some(distinct) = self.distinct {
            params.push(format!("distinct={}", if distinct { "true" } else { "false" }));
        }
        if !self.attributes_to_retrieve.is_empty() {
            let encoded =
                serde_json::to_string(&self.attributes_to_retrieve).unwrap_or_default();
            params.push(format!(
                "attributesToRetrieve={}",
                urlencoding::encode(&encoded)
            ));
        }
        params.join("&")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub hits: Vec<Value>,
    #[serde(default, rename = "nbHits")]
    pub nb_hits: usize,
    #[serde(default)]
    pub facets: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowseResponse {
    #[serde(default)]
    pub hits: Vec<Value>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FacetHit {
    pub value: String,
    pub count: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct FacetSearchResponse {
    #[serde(default, rename = "facetHits")]
    facet_hits: Vec<FacetHit>,
}

impl AlgoliaClient {
    pub fn new(app_id: &str, api_key: &str, base_url: Option<&str>) -> Self {
        let base_url = base_url
            .map(|url| url.trim_end_matches('/').to_string())
            .unwrap_or_else(|| format!("https://{}.algolia.net", app_id.to_lowercase()));
        Self {
            http: reqwest::Client::new(),
            app_id: app_id.to_string(),
            api_key: api_key.to_string(),
            base_url,
        }
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Algolia-Application-Id", &self.app_id)
            .header("X-Algolia-API-Key", &self.api_key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let payload: Value = response.json().await.map_err(|error| {
            AlgoliaError::InvalidResponse(format!("non-JSON response from {url}: {error}"))
        })?;
        if !status.is_success() {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Err(AlgoliaError::Api {
                status: status.as_u16(),
                message,
            });
        }
        Ok(payload)
    }

    fn index_path(index: &str, suffix: &str) -> String {
        format!("/1/indexes/{}{}", urlencoding::encode(index), suffix)
    }

    /// Batch write: `addObject` upserts (with objectID), `deleteObject` removes.
    pub async fn batch(&self, index: &str, requests: Vec<Value>) -> Result<Value> {
        self.request(
            reqwest::Method::POST,
            &Self::index_path(index, "/batch"),
            Some(json!({ "requests": requests })),
        )
        .await
    }

    pub async fn save_objects(&self, index: &str, objects: Vec<Value>) -> Result<Value> {
        let requests: Vec<Value> = objects
            .into_iter()
            .map(|body| json!({ "action": "addObject", "body": body }))
            .collect();
        self.batch(index, requests).await
    }

    pub async fn delete_objects(&self, index: &str, object_ids: Vec<String>) -> Result<Value> {
        let requests: Vec<Value> = object_ids
            .into_iter()
            .map(|id| json!({ "action": "deleteObject", "body": { "objectID": id } }))
            .collect();
        self.batch(index, requests).await
    }

    pub async fn search(&self, index: &str, request: &SearchRequest) -> Result<SearchResponse> {
        let payload = self
            .request(
                reqwest::Method::POST,
                &Self::index_path(index, "/query"),
                Some(json!({ "params": request.to_params() })),
            )
            .await?;
        serde_json::from_value(payload)
            .map_err(|error| AlgoliaError::InvalidResponse(error.to_string()))
    }

    /// Algolia's hard ceiling for `maxFacetHits` on `searchForFacetValues`.
    /// Sending more is a 400, not a clamp — and it is easy to confuse with
    /// `maxValuesPerFacet`, whose ceiling is 1,000.
    pub const MAX_FACET_HITS: usize = 100;

    /// Facet-value search. `max_facet_hits` is clamped to [`Self::MAX_FACET_HITS`];
    /// when the response comes back full the result may be truncated —
    /// [`Self::search_facet_values_checked`] surfaces that.
    pub async fn search_facet_values(
        &self,
        index: &str,
        facet: &str,
        facet_query: &str,
        filters: Option<&str>,
        max_facet_hits: usize,
    ) -> Result<Vec<FacetHit>> {
        let max_facet_hits = max_facet_hits.clamp(1, Self::MAX_FACET_HITS);
        let mut params = format!(
            "facetQuery={}&maxFacetHits={max_facet_hits}",
            urlencoding::encode(facet_query)
        );
        if let Some(filters) = filters {
            params.push_str(&format!("&filters={}", urlencoding::encode(filters)));
        }
        let path = Self::index_path(index, &format!("/facets/{}/query", urlencoding::encode(facet)));
        let payload = self
            .request(reqwest::Method::POST, &path, Some(json!({ "params": params })))
            .await?;
        let response: FacetSearchResponse = serde_json::from_value(payload)
            .map_err(|error| AlgoliaError::InvalidResponse(error.to_string()))?;
        Ok(response.facet_hits)
    }

    /// Like [`Self::search_facet_values`], plus `true` when the response filled
    /// the capped budget and values may therefore have been dropped. Callers
    /// that enumerate structure report this rather than silently under-listing.
    pub async fn search_facet_values_checked(
        &self,
        index: &str,
        facet: &str,
        facet_query: &str,
        filters: Option<&str>,
    ) -> Result<(Vec<FacetHit>, bool)> {
        let hits = self
            .search_facet_values(index, facet, facet_query, filters, Self::MAX_FACET_HITS)
            .await?;
        let truncated = hits.len() >= Self::MAX_FACET_HITS;
        Ok((hits, truncated))
    }

    pub async fn get_settings(&self, index: &str) -> Result<Value> {
        self.request(
            reqwest::Method::GET,
            &Self::index_path(index, "/settings"),
            None,
        )
        .await
    }

    pub async fn set_settings(&self, index: &str, settings: Value) -> Result<Value> {
        self.request(
            reqwest::Method::PUT,
            &Self::index_path(index, "/settings"),
            Some(settings),
        )
        .await
    }

    pub async fn delete_by_query(&self, index: &str, filters: &str) -> Result<Value> {
        self.request(
            reqwest::Method::POST,
            &Self::index_path(index, "/deleteByQuery"),
            Some(json!({ "params": format!("filters={}", urlencoding::encode(filters)) })),
        )
        .await
    }

    /// Fetch specific objects by ID. Missing objects come back as `None`.
    pub async fn get_objects(
        &self,
        index: &str,
        object_ids: &[String],
    ) -> Result<Vec<Option<Value>>> {
        let requests: Vec<Value> = object_ids
            .iter()
            .map(|id| json!({ "indexName": index, "objectID": id }))
            .collect();
        let payload = self
            .request(
                reqwest::Method::POST,
                "/1/indexes/*/objects",
                Some(json!({ "requests": requests })),
            )
            .await?;
        let results = payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| AlgoliaError::InvalidResponse("missing results".to_string()))?;
        Ok(results
            .iter()
            .map(|value| if value.is_null() { None } else { Some(value.clone()) })
            .collect())
    }

    /// Browse every record matching `filters`, following cursors to exhaustion.
    pub async fn browse_all(&self, index: &str, filters: Option<&str>) -> Result<Vec<Value>> {
        let mut hits = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = serde_json::Map::new();
            let mut params = "hitsPerPage=1000".to_string();
            if let Some(filters) = filters {
                params.push_str(&format!("&filters={}", urlencoding::encode(filters)));
            }
            body.insert("params".to_string(), Value::String(params));
            if let Some(cursor) = &cursor {
                body.insert("cursor".to_string(), Value::String(cursor.clone()));
            }
            let payload = self
                .request(
                    reqwest::Method::POST,
                    &Self::index_path(index, "/browse"),
                    Some(Value::Object(body)),
                )
                .await?;
            let response: BrowseResponse = serde_json::from_value(payload)
                .map_err(|error| AlgoliaError::InvalidResponse(error.to_string()))?;
            hits.extend(response.hits);
            match response.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(hits)
    }
}

/// Generates a secured API key: `base64(hex(hmac_sha256(parent_key,
/// restrictions)) + restrictions)` where `restrictions` is a url-encoded
/// query-parameter string (e.g. `filters=folders.lvl0%3A_Wiki`). Validated
/// server-side by Algolia; no API call involved.
pub fn generate_secured_api_key(parent_key: &str, restrictions: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(parent_key.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(restrictions.as_bytes());
    let signature = mac.finalize().into_bytes();
    let hex_signature: String = signature.iter().map(|byte| format!("{byte:02x}")).collect();
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("{hex_signature}{restrictions}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_request_serializes_to_params() {
        let request = SearchRequest {
            query: "deep obsidian".to_string(),
            filters: Some("recordType:chunk AND dir:\"_Wiki\"".to_string()),
            hits_per_page: Some(20),
            distinct: Some(false),
            ..SearchRequest::default()
        };
        let params = request.to_params();
        assert!(params.contains("query=deep%20obsidian"));
        assert!(params.contains("hitsPerPage=20"));
        assert!(params.contains("distinct=false"));
        assert!(params.contains("filters="));
    }

    #[test]
    fn secured_key_embeds_restrictions() {
        let key = generate_secured_api_key("parent-key", "filters=folders.lvl0%3A_Wiki");
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .expect("valid base64");
        let decoded = String::from_utf8(decoded).expect("utf8");
        // 64 hex chars of HMAC-SHA256, then the restriction string verbatim.
        assert_eq!(decoded.len(), 64 + "filters=folders.lvl0%3A_Wiki".len());
        assert!(decoded.ends_with("filters=folders.lvl0%3A_Wiki"));
    }
}
