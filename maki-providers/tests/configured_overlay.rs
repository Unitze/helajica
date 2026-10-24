//! What a `providers.toml` entry means for a slug a declaration serves.
//!
//! Out of line rather than beside [`maki_providers::plugin`]'s own tests
//! because the config is read from the process-wide home directory: each case
//! needs its own, and `cargo nextest` is what gives a test its own process.

use std::fs;
use std::sync::Arc;

use maki_config::providers::{Protocol, ProvidersConfig};
use maki_providers::plugin::{
    self, DeclAuthority, PlanDecl, ProviderDecl, RegisterError, Registration,
};
use maki_providers::provider::provider_for_slug;
use maki_providers::spec::Owner;
use maki_providers::{Timeouts, catalog_provider_if_available, warm_catalog};
use maki_storage::StateDir;
use maki_storage::auth::{ProviderCredentials, save_provider_credentials};
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;

const HOME_VARS: &[&str] = &[
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
];

const PROVIDERS_FILE: &str = "providers.toml";

const DECLARED_SLUG: &str = "overlaid-provider";
const DECLARED_HOST: &str = "api.overlaid.example";
const DECLARED_URL: &str = "https://api.overlaid.example/v1";
const DECLARED_DISPLAY_NAME: &str = "Overlaid";
const GATEWAY_URL: &str = "https://gateway.example/v1";
const LONE_KEY: &str = "discover_models = true";

const GATEWAY_HEADER: &str = "X-Gateway-Key";
const UNSET_VAR: &str = "MAKI_TEST_UNSET_GATEWAY_KEY";

const OWN_SLUG: &str = "configured-provider";
const CONFIGURED_URL: &str = "https://configured.example/v1";
const OWN_HOST: &str = "declared.example";
const OWN_DISPLAY_NAME: &str = "Configured";

const TEMPDIR_FAILED: &str = "no temporary state directory";
const CONFIG_DIR_FAILED: &str = "the isolated config directory did not resolve";
const WRITE_FAILED: &str = "providers.toml could not be written";
const PROVIDER_LOST: &str = "a providers.toml overlay took the slug off its declaration";
const BUILT_DESPITE_BAD_HEADER: &str = "a bad [<slug>.headers] must fail the build";

const PLAN_KEY: &str = "coding";
const PLAN_HOST: &str = "plan.overlaid.example";
const PLAN_KEY_ENV: &str = "MAKI_TEST_PLANNED_PROVIDER_KEY";
const CATALOG_SLUG: &str = "selected-catalog";
const CATALOG_FILE: &str = "models-dev-catalog.json";
const SELECTED_OWNER: &str = "selected-package";
const OTHER_OWNER: &str = "other-package";
const SAVED_KEY: &str = "sk-selected-catalog";
const SAVED_BEARER: &str = "Bearer sk-selected-catalog";
const KEY_ENV: &str = "MAKI_TEST_SELECTED_CATALOG_KEY";
const STALE_MODEL: &str = "stale-model";

/// Points every base directory at a throwaway tree holding `providers.toml`,
/// so no case reads this machine's config. The file goes wherever maki's own
/// path resolution says it lives, so a test can never quietly write somewhere
/// nothing reads.
fn isolated(providers_toml: &str) -> TempDir {
    let dir = TempDir::new().expect(TEMPDIR_FAILED);
    for var in HOME_VARS {
        unsafe { std::env::set_var(var, dir.path()) };
    }
    let config = maki_storage::paths::config_dir().expect(CONFIG_DIR_FAILED);
    fs::write(config.join(PROVIDERS_FILE), providers_toml).expect(WRITE_FAILED);
    dir
}

fn own_definition() -> String {
    format!("protocol = \"openai\"\nbase_url = \"{CONFIGURED_URL}\"")
}

fn declaration(slug: &str, display_name: &str, host: &str) -> Registration {
    Registration {
        decl: ProviderDecl {
            slug: slug.to_owned(),
            display_name: display_name.to_owned(),
            codec: Some(Protocol::Openai),
            base_url: Some(format!("https://{host}/v1")),
            net_hosts: vec![host.to_owned()],
            ..ProviderDecl::default()
        },
        hooks: plugin::ProviderHooks::default(),
    }
}

fn load_bundled() {
    plugin::begin_load();
    plugin::register(
        declaration(DECLARED_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST),
        DeclAuthority::Bundled,
    )
    .expect(PROVIDER_LOST);
    plugin::commit_load();
}

/// Users write `[deepseek] base_url = ...` to send a bundled provider through
/// their gateway. If that entry cost the plugin its slug, they would get a
/// bare models.dev stub instead of the provider they meant to tweak. So the
/// declaration stays, whatever the entry says. Even a `protocol` only earns a
/// warning, because refusing the bundled load would stop maki from starting.
#[test_case(&format!("base_url = \"{GATEWAY_URL}\""), GATEWAY_URL ; "a_gateway")]
#[test_case(LONE_KEY, DECLARED_URL ; "a_lone_key")]
#[test_case(&own_definition(), CONFIGURED_URL ; "a_whole_definition")]
fn an_entry_keeps_the_bundled_declaration(entry: &str, origin: &str) {
    let _home = isolated(&format!("[{DECLARED_SLUG}]\n{entry}\n"));
    load_bundled();

    assert!(
        matches!(Owner::of(DECLARED_SLUG), Owner::Plugin),
        "{PROVIDER_LOST}"
    );
    assert_eq!(
        plugin::effective_base_url(DECLARED_SLUG).as_deref(),
        Some(origin)
    );
}

/// A bad `[<slug>.headers]` only fails the provider when it is built. Failing
/// the bundled load instead would stop maki from starting, over a provider the
/// user may never pick.
#[test]
fn a_bad_header_on_a_bundled_slug_fails_only_its_build() {
    let _home = isolated(&format!(
        "[{DECLARED_SLUG}.headers]\n\"{GATEWAY_HEADER}\" = \"${{{UNSET_VAR}}}\"\n"
    ));
    unsafe { std::env::remove_var(UNSET_VAR) };
    load_bundled();

    assert!(
        matches!(Owner::of(DECLARED_SLUG), Owner::Plugin),
        "{PROVIDER_LOST}"
    );
    let error = plugin::create(DECLARED_SLUG, Timeouts::default())
        .err()
        .expect(BUILT_DESPITE_BAD_HEADER)
        .to_string();
    assert!(
        error.contains(GATEWAY_HEADER) && error.contains(UNSET_VAR),
        "{error}"
    );
}

/// With `plan = "..."` the user picks, but the plugin wrote every origin on
/// the menu. So the origin is only trusted when maki itself wrote it.
#[test_case(DeclAuthority::ThirdParty, false ; "third_party")]
#[test_case(DeclAuthority::Bundled, true ; "bundled")]
fn a_planned_origin_is_vouched_only_for_a_bundled_declaration(
    authority: DeclAuthority,
    vouched: bool,
) {
    let _home = isolated(&format!("[{DECLARED_SLUG}]\nplan = \"{PLAN_KEY}\"\n"));
    let plan_url = format!("https://{PLAN_HOST}/v1");
    let mut reg = declaration(DECLARED_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST);
    reg.decl.api_key_env = Some(PLAN_KEY_ENV.to_owned());
    reg.decl.net_hosts.push(PLAN_HOST.to_owned());
    reg.decl.plans = vec![PlanDecl {
        key: PLAN_KEY.to_owned(),
        display_name: DECLARED_DISPLAY_NAME.to_owned(),
        base_url: Some(plan_url.clone()),
        default_model: None,
        login_url: None,
    }];
    plugin::begin_load();
    plugin::register(reg, authority).expect(PROVIDER_LOST);
    plugin::commit_load();

    assert_eq!(
        plugin::effective_base_url(DECLARED_SLUG).as_deref(),
        Some(plan_url.as_str())
    );
    assert_eq!(plugin::vouched_origin(DECLARED_SLUG).is_some(), vouched);
}

/// An entry with a `protocol` is a whole provider already. A third-party
/// plugin taking the same slug would be a second provider with the same name.
#[test]
fn a_slug_providers_toml_defines_is_still_refused() {
    let _home = isolated(&format!("[{OWN_SLUG}]\n{}\n", own_definition()));
    plugin::begin_load();
    let error = plugin::register(
        declaration(OWN_SLUG, OWN_DISPLAY_NAME, OWN_HOST),
        DeclAuthority::ThirdParty,
    )
    .unwrap_err();
    plugin::commit_load();

    assert!(matches!(error, RegisterError::ConfiguredSlug(_)), "{error}");
}

fn selected_catalog() -> TempDir {
    let home = isolated(&format!("[{CATALOG_SLUG}]\nplugin = {SELECTED_OWNER:?}\n"));
    let cache = maki_storage::paths::cache_dir().unwrap();
    fs::write(
        cache.join(CATALOG_FILE),
        json!({
            CATALOG_SLUG: {
                "name": DECLARED_DISPLAY_NAME,
                "npm": "@ai-sdk/openai-compatible",
                "api": DECLARED_URL,
                "env": [],
                "models": { STALE_MODEL: {} },
            }
        })
        .to_string(),
    )
    .unwrap();
    save_provider_credentials(
        &StateDir::resolve().unwrap(),
        CATALOG_SLUG,
        &ProviderCredentials {
            api_key: SAVED_KEY.into(),
            host: None,
        },
    )
    .unwrap();
    unsafe { std::env::remove_var(KEY_ENV) };
    home
}

fn select_owner(owner: Option<&str>) {
    let mut config = ProvidersConfig::load();
    config.providers.get_mut(CATALOG_SLUG).unwrap().plugin = owner.map(str::to_owned);
    config.save().unwrap();
}

fn register_selected() {
    let mut reg = declaration(CATALOG_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST);
    reg.decl.api_key_env = Some(KEY_ENV.into());
    plugin::register_plugin(Arc::from(SELECTED_OWNER), reg, DeclAuthority::ThirdParty).unwrap();
}

#[test_case(None ; "unowned")]
#[test_case(Some(OTHER_OWNER) ; "another_package")]
fn a_selected_catalog_name_refuses_other_owners(owner: Option<&str>) {
    let _home = selected_catalog();
    plugin::begin_load();
    let reg = declaration(CATALOG_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST);
    let result = match owner {
        Some(owner) => plugin::register_plugin(Arc::from(owner), reg, DeclAuthority::ThirdParty),
        None => plugin::register(reg, DeclAuthority::ThirdParty),
    };
    let error = result.unwrap_err();
    plugin::commit_load();
    assert!(
        matches!(error, RegisterError::SelectedOwner { .. }),
        "{error}"
    );
    assert!(!plugin::is_registered(CATALOG_SLUG));
    assert!(plugin::resolved_auth(CATALOG_SLUG).is_none());
}

#[test]
fn selection_changes_take_effect_only_after_reload() {
    let _home = selected_catalog();
    plugin::begin_load();
    register_selected();
    plugin::commit_load();
    provider_for_slug(CATALOG_SLUG, Timeouts::default()).unwrap();
    assert!(matches!(Owner::of(CATALOG_SLUG), Owner::Plugin));
    assert_eq!(
        plugin::resolved_auth(CATALOG_SLUG).unwrap().headers,
        [("authorization".into(), SAVED_BEARER.into())]
    );

    select_owner(Some(OTHER_OWNER));
    assert!(plugin::spec(CATALOG_SLUG).is_some());
    assert!(plugin::resolved_auth(CATALOG_SLUG).is_some());
    provider_for_slug(CATALOG_SLUG, Timeouts::default()).unwrap();
    plugin::begin_load();
    assert!(plugin::is_registered(CATALOG_SLUG));
    plugin::commit_load();
    assert!(!plugin::is_registered(CATALOG_SLUG));
    let error = provider_for_slug(CATALOG_SLUG, Timeouts::default())
        .err()
        .unwrap();
    assert!(error.to_string().contains(OTHER_OWNER), "{error}");

    select_owner(None);
    assert!(matches!(Owner::of(CATALOG_SLUG), Owner::Plugin));
    assert!(provider_for_slug(CATALOG_SLUG, Timeouts::default()).is_err());
    plugin::begin_load();
    plugin::commit_load();
    assert!(!matches!(Owner::of(CATALOG_SLUG), Owner::Plugin));
    let fallback = provider_for_slug(CATALOG_SLUG, Timeouts::default()).unwrap();
    let models = smol::block_on(fallback.list_models()).unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        [STALE_MODEL]
    );
}

#[test_case(false ; "missing")]
#[test_case(true ; "failed_load")]
fn an_unavailable_selected_owner_cannot_fall_back_to_catalog_requests(failed: bool) {
    let _home = selected_catalog();
    plugin::begin_load();
    if failed {
        register_selected();
        plugin::discard(SELECTED_OWNER);
    }
    plugin::commit_load();
    assert!(!plugin::is_registered(CATALOG_SLUG));
    assert!(matches!(Owner::of(CATALOG_SLUG), Owner::Plugin));
    let error = provider_for_slug(CATALOG_SLUG, Timeouts::default())
        .err()
        .unwrap();
    assert!(error.to_string().contains(SELECTED_OWNER), "{error}");
}

#[test]
fn an_owner_changed_during_load_takes_effect_on_the_next_load() {
    let _home = selected_catalog();
    plugin::begin_load();
    select_owner(Some(OTHER_OWNER));
    register_selected();
    plugin::commit_load();
    assert!(plugin::resolved_auth(CATALOG_SLUG).is_some());
    provider_for_slug(CATALOG_SLUG, Timeouts::default()).unwrap();
    plugin::begin_load();
    let error = plugin::register_plugin(
        Arc::from(SELECTED_OWNER),
        declaration(CATALOG_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST),
        DeclAuthority::ThirdParty,
    )
    .unwrap_err();
    assert!(
        matches!(error, RegisterError::SelectedOwner { .. }),
        "{error}"
    );
    plugin::commit_load();
    assert!(provider_for_slug(CATALOG_SLUG, Timeouts::default()).is_err());
}

#[test_case("anthropic" ; "compiled")]
#[test_case("deepseek" ; "bundled")]
fn selecting_a_package_does_not_release_a_shipped_name(slug: &str) {
    let _home = isolated(&format!("[{slug}]\nplugin = {SELECTED_OWNER:?}\n"));
    plugin::begin_load();
    let error = plugin::register_plugin(
        Arc::from(SELECTED_OWNER),
        declaration(slug, DECLARED_DISPLAY_NAME, DECLARED_HOST),
        DeclAuthority::ThirdParty,
    )
    .unwrap_err();
    plugin::commit_load();
    assert!(matches!(error, RegisterError::ReservedSlug(_)), "{error}");
}

#[test]
fn warming_a_new_catalog_does_not_revoke_a_loaded_provider() {
    let _home = selected_catalog();
    select_owner(None);
    let cache = maki_storage::paths::cache_dir().unwrap().join(CATALOG_FILE);
    let catalog_json = fs::read_to_string(&cache).unwrap();
    fs::write(&cache, "{}").unwrap();
    plugin::begin_load();
    register_selected();
    plugin::commit_load();
    fs::write(&cache, catalog_json).unwrap();
    warm_catalog();
    assert!(catalog_provider_if_available(CATALOG_SLUG).is_some());
    provider_for_slug(CATALOG_SLUG, Timeouts::default()).unwrap();
    assert!(plugin::spec(CATALOG_SLUG).is_some());
    assert!(plugin::resolved_auth(CATALOG_SLUG).is_some());

    plugin::begin_load();
    let error = plugin::register_plugin(
        Arc::from(SELECTED_OWNER),
        declaration(CATALOG_SLUG, DECLARED_DISPLAY_NAME, DECLARED_HOST),
        DeclAuthority::ThirdParty,
    )
    .unwrap_err();
    assert!(matches!(error, RegisterError::ReservedSlug(_)), "{error}");
    plugin::commit_load();
    assert!(!plugin::is_registered(CATALOG_SLUG));
}
