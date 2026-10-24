//! Providers connected in Settings. Each one is an entry in maki's
//! `providers.toml` and a key in maki's credential store, so the terminal
//! `maki` sees the same providers.

use std::time::Duration;

use isahc::config::Configurable as _;
use isahc::{ReadResponseExt as _, Request, RequestExt as _};
use maki_config::providers::{
    ModelDef, ModelTier, Protocol, ProvidersConfig, builtin_provider, slugify,
};
use maki_providers::catalog_providers;
use maki_storage::StateDir;
use maki_storage::auth::{
    ProviderCredentials, delete_provider_credentials, load_provider_credentials,
    save_provider_credentials,
};
use serde_json::Value;

pub const PROTOCOLS: [(Protocol, &str); 4] = [
    (Protocol::Openai, "OpenAI-compatible"),
    (Protocol::OpenaiResponses, "OpenAI Responses"),
    (Protocol::Anthropic, "Anthropic"),
    (Protocol::Google, "Google Gemini"),
];
const SECURE_SCHEME: &str = "https://";
const PLAIN_SCHEME: &str = "http://";
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
const MODELS_PATH: &str = "/models";
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const ERROR_BODY_CHARS: usize = 200;
const ANTHROPIC_VERSION: &str = "2023-06-01";
const GOOGLE_MODEL_PREFIX: &str = "models/";
/// Where `/models` listings put a model's context window, by API.
const CONTEXT_FIELDS: [&str; 6] = [
    "context_length",
    "context_window",
    "max_context_length",
    "max_model_len",
    "max_input_tokens",
    "inputTokenLimit",
];
const THOUSAND: f64 = 1_000.;
const MILLION: f64 = 1_000_000.;

/// A model of a provider, with the context window maki should assume for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModel {
    pub id: String,
    pub context_window: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Connected {
    pub slug: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub models: Vec<ProviderModel>,
    pub has_key: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub name: String,
    pub protocol: Option<Protocol>,
    pub base_url: String,
    pub api_key: String,
    /// Empty asks the provider's model list instead.
    pub models: Vec<ProviderModel>,
}

pub fn protocol_label(protocol: Protocol) -> &'static str {
    PROTOCOLS
        .iter()
        .find(|(p, _)| *p == protocol)
        .map_or("", |(_, label)| label)
}

/// The custom entries, by name. Built-in overrides are left out: they are
/// not something the user connected here.
pub fn list(storage: Option<&StateDir>) -> Vec<Connected> {
    let mut connected: Vec<Connected> = ProvidersConfig::load_or_default()
        .providers
        .into_iter()
        .filter_map(|(slug, def)| {
            Some(Connected {
                name: def.display_name.clone().unwrap_or_else(|| slug.clone()),
                protocol: def.protocol?,
                base_url: def.base_url.clone().unwrap_or_default(),
                models: def
                    .models
                    .iter()
                    .map(|m| ProviderModel {
                        id: m.id.clone(),
                        context_window: m.context_window,
                    })
                    .collect(),
                has_key: def.api_key.is_some()
                    || storage.is_some_and(|s| load_provider_credentials(s, &slug).is_some()),
                slug,
            })
        })
        .collect();
    connected.sort_by_key(|c| c.name.to_lowercase());
    connected
}

/// Adds the provider, or updates the one with the same name. A blank key or
/// model list keeps what was saved before.
pub fn connect(draft: &Draft, storage: Option<&StateDir>) -> Result<String, String> {
    let (slug, protocol, base_url) = validate(draft)?;
    let mut config = ProvidersConfig::load_or_default();
    let mut def = config.get(&slug).cloned().unwrap_or_default();
    def.display_name = Some(draft.name.trim().to_owned());
    def.protocol = Some(protocol);
    def.base_url = Some(base_url);
    if let Some(first) = draft.models.first() {
        def.default_model = Some(format!("{slug}/{}", first.id));
        let saved = std::mem::take(&mut def.models);
        def.models = draft
            .models
            .iter()
            .map(|wanted| {
                let mut def = saved
                    .iter()
                    .find(|m| m.id == wanted.id)
                    .cloned()
                    .unwrap_or_else(|| model(wanted.id.clone()));
                def.context_window = wanted.context_window;
                def
            })
            .collect();
    }
    def.discover_models = def.models.is_empty();
    let key = draft.api_key.trim();
    if !key.is_empty() {
        let storage = storage.ok_or("No place to keep the API key")?;
        let credentials = ProviderCredentials {
            api_key: key.to_owned(),
            host: None,
        };
        save_provider_credentials(storage, &slug, &credentials)
            .map_err(|e| format!("Save the API key: {e}"))?;
    }
    config.upsert(slug.clone(), def);
    config
        .save()
        .map_err(|e| format!("Save providers.toml: {e}"))?;
    Ok(slug)
}

pub fn disconnect(slug: &str, storage: Option<&StateDir>) -> Result<(), String> {
    let mut config = ProvidersConfig::load_or_default();
    config.remove(slug);
    config
        .save()
        .map_err(|e| format!("Save providers.toml: {e}"))?;
    if let Some(storage) = storage {
        delete_provider_credentials(storage, slug)
            .map_err(|e| format!("Remove the API key: {e}"))?;
    }
    Ok(())
}

/// Asks the provider's `/models` for what it serves, with the context window
/// it reports or, failing that, the one models.dev knows.
pub fn fetch_models(
    draft: &Draft,
    storage: Option<&StateDir>,
) -> Result<Vec<ProviderModel>, String> {
    let (slug, protocol, base_url) = validate(draft)?;
    let key = Some(draft.api_key.trim().to_owned())
        .filter(|k| !k.is_empty())
        .or_else(|| {
            storage
                .and_then(|s| load_provider_credentials(s, &slug))
                .map(|c| c.api_key)
        });
    let mut request = Request::get(format!("{base_url}{MODELS_PATH}")).timeout(FETCH_TIMEOUT);
    if let Some(key) = &key {
        request = match protocol {
            Protocol::Anthropic => request
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION),
            Protocol::Google => request.header("x-goog-api-key", key),
            _ => request.header("authorization", format!("Bearer {key}")),
        };
    }
    let mut response = request
        .body(())
        .map_err(|e| e.to_string())?
        .send()
        .map_err(|e| format!("Could not reach {base_url}: {e}"))?;
    let text = response.text().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let excerpt: String = text.chars().take(ERROR_BODY_CHARS).collect();
        return Err(format!(
            "{MODELS_PATH} answered {}: {excerpt}",
            response.status()
        ));
    }
    let listing: Value =
        serde_json::from_str(&text).map_err(|e| format!("{MODELS_PATH} is not JSON: {e}"))?;
    let mut models = parse_listing(&listing);
    if models.iter().any(|m| m.context_window.is_none()) {
        let catalog = catalog_providers();
        for model in models.iter_mut().filter(|m| m.context_window.is_none()) {
            model.context_window = catalog
                .iter()
                .find_map(|p| p.models.get(&model.id).and_then(|meta| meta.context));
        }
    }
    Ok(models)
}

/// OpenAI-style `{"data": [...]}` (Anthropic too) or Gemini's
/// `{"models": [...]}`, sorted by id.
fn parse_listing(listing: &Value) -> Vec<ProviderModel> {
    let rows = listing["data"]
        .as_array()
        .or_else(|| listing["models"].as_array())
        .or_else(|| listing.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut models: Vec<ProviderModel> = rows
        .iter()
        .filter_map(|row| {
            let id = row["id"].as_str().or_else(|| {
                row["name"]
                    .as_str()
                    .map(|n| n.strip_prefix(GOOGLE_MODEL_PREFIX).unwrap_or(n))
            })?;
            let context_window = CONTEXT_FIELDS
                .iter()
                .find_map(|field| row[*field].as_u64())
                .and_then(|n| u32::try_from(n).ok());
            Some(ProviderModel {
                id: id.to_owned(),
                context_window,
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models
}

/// `128000`, `128k` or `1M`.
pub fn parse_tokens(text: &str) -> Option<u32> {
    let text = text.trim().to_lowercase();
    let (number, scale) = match text.strip_suffix('k') {
        Some(n) => (n, THOUSAND),
        None => match text.strip_suffix('m') {
            Some(n) => (n, MILLION),
            None => (text.as_str(), 1.),
        },
    };
    let value = number.trim().parse::<f64>().ok()? * scale;
    (value >= 1. && value <= f64::from(u32::MAX)).then_some(value.round() as u32)
}

fn validate(draft: &Draft) -> Result<(String, Protocol, String), String> {
    let slug = slugify(&draft.name);
    if slug.is_empty() {
        return Err("Give the provider a name".into());
    }
    if builtin_provider(&slug).is_some() {
        return Err(format!(
            "\"{slug}\" is a built-in provider; pick another name"
        ));
    }
    let protocol = draft.protocol.ok_or("Pick the API the provider speaks")?;
    let base_url = draft.base_url.trim().trim_end_matches('/').to_owned();
    if !allowed_url(&base_url) {
        return Err("The base URL must be https://, or http:// on this machine".into());
    }
    Ok((slug, protocol, base_url))
}

fn allowed_url(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix(SECURE_SCHEME) {
        return !rest.is_empty();
    }
    url.strip_prefix(PLAIN_SCHEME).is_some_and(|rest| {
        LOOPBACK_HOSTS.iter().any(|host| {
            rest.strip_prefix(host)
                .is_some_and(|tail| tail.is_empty() || tail.starts_with([':', '/']))
        })
    })
}

fn model(id: String) -> ModelDef {
    ModelDef {
        id,
        tier: ModelTier::default(),
        context_window: None,
        max_output_tokens: None,
        supports_tool_examples: None,
        supports_thinking: None,
        requires_thinking: None,
        thinking_fields: None,
        supports_vision: None,
        pricing_input: None,
        pricing_output: None,
        pricing_cache_write: None,
        pricing_cache_read: None,
        pricing_fast_input: None,
        pricing_fast_output: None,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const NAME: &str = "My Gateway";
    const URL: &str = "https://llm.example.com/v1";

    fn draft(name: &str, protocol: Option<Protocol>, base_url: &str) -> Draft {
        Draft {
            name: name.into(),
            protocol,
            base_url: base_url.into(),
            ..Draft::default()
        }
    }

    #[test]
    fn slugs_the_name_and_trims_the_url() {
        let checked = validate(&draft(
            NAME,
            Some(Protocol::Openai),
            "https://llm.example.com/v1/",
        ));
        assert_eq!(
            checked,
            Ok(("my-gateway".into(), Protocol::Openai, URL.into()))
        );
    }

    #[test_case("", Some(Protocol::Openai), URL ; "no_name")]
    #[test_case(NAME, None, URL ; "no_protocol")]
    #[test_case(NAME, Some(Protocol::Openai), "http://llm.example.com" ; "plain_http_off_machine")]
    #[test_case(NAME, Some(Protocol::Openai), "ftp://x" ; "other_scheme")]
    #[test_case(NAME, Some(Protocol::Openai), "http://localhost.evil.com" ; "lookalike_host")]
    fn refuses_bad_drafts(name: &str, protocol: Option<Protocol>, url: &str) {
        assert!(validate(&draft(name, protocol, url)).is_err());
    }

    #[test_case("http://localhost:11434/v1" ; "localhost_with_port")]
    #[test_case("http://127.0.0.1" ; "ipv4_loopback")]
    #[test_case("http://[::1]:8080" ; "ipv6_loopback")]
    fn allows_plain_http_on_this_machine(url: &str) {
        assert!(allowed_url(url));
    }

    #[test_case("128000", Some(128_000) ; "plain")]
    #[test_case("128k", Some(128_000) ; "thousands")]
    #[test_case(" 1M ", Some(1_000_000) ; "millions")]
    #[test_case("1.5m", Some(1_500_000) ; "fractional")]
    #[test_case("", None ; "blank")]
    #[test_case("lots", None ; "words")]
    #[test_case("0", None ; "zero")]
    fn token_counts_parse(text: &str, expected: Option<u32>) {
        assert_eq!(parse_tokens(text), expected);
    }

    #[test_case(r#"{"data":[{"id":"b","context_length":8192},{"id":"a"}]}"#, &[("a", None), ("b", Some(8192))] ; "openai")]
    #[test_case(r#"{"data":[{"id":"claude","max_input_tokens":200000}]}"#, &[("claude", Some(200_000))] ; "anthropic")]
    #[test_case(r#"{"models":[{"name":"models/gemini","inputTokenLimit":1048576}]}"#, &[("gemini", Some(1_048_576))] ; "gemini")]
    #[test_case(r#"{"error":"nope"}"#, &[] ; "not_a_listing")]
    fn listings_parse(json: &str, expected: &[(&str, Option<u32>)]) {
        let models = parse_listing(&serde_json::from_str(json).unwrap());
        let got: Vec<(&str, Option<u32>)> = models
            .iter()
            .map(|m| (m.id.as_str(), m.context_window))
            .collect();
        assert_eq!(got, expected);
    }
}
