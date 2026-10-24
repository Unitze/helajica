//! Providers connected in Settings. Each one is an entry in maki's
//! `providers.toml` and a key in maki's credential store, so the terminal
//! `maki` sees the same providers.

use maki_config::providers::{
    ModelDef, ModelTier, Protocol, ProvidersConfig, builtin_provider, slugify,
};
use maki_storage::StateDir;
use maki_storage::auth::{
    ProviderCredentials, delete_provider_credentials, load_provider_credentials,
    save_provider_credentials,
};

pub const PROTOCOLS: [(Protocol, &str); 4] = [
    (Protocol::Openai, "OpenAI-compatible"),
    (Protocol::OpenaiResponses, "OpenAI Responses"),
    (Protocol::Anthropic, "Anthropic"),
    (Protocol::Google, "Google Gemini"),
];
const SECURE_SCHEME: &str = "https://";
const PLAIN_SCHEME: &str = "http://";
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
const MODEL_SEPARATORS: [char; 2] = [',', '\n'];

#[derive(Debug, Clone, PartialEq)]
pub struct Connected {
    pub slug: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub models: Vec<String>,
    pub has_key: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub name: String,
    pub protocol: Option<Protocol>,
    pub base_url: String,
    pub api_key: String,
    /// Model ids separated by commas or lines. Empty asks the provider's
    /// model list instead.
    pub models: String,
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
                models: def.models.iter().map(|m| m.id.clone()).collect(),
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
    let models = parse_models(&draft.models);
    let mut config = ProvidersConfig::load_or_default();
    let mut def = config.get(&slug).cloned().unwrap_or_default();
    def.display_name = Some(draft.name.trim().to_owned());
    def.protocol = Some(protocol);
    def.base_url = Some(base_url);
    if !models.is_empty() {
        def.default_model = Some(format!("{slug}/{}", models[0]));
        def.models = models.into_iter().map(model).collect();
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

fn parse_models(text: &str) -> Vec<String> {
    text.split(MODEL_SEPARATORS)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
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

    #[test]
    fn reads_models_from_commas_and_lines() {
        assert_eq!(
            parse_models(" fast-v1, smart-v1\n\nlocal "),
            ["fast-v1", "smart-v1", "local"]
        );
    }
}
