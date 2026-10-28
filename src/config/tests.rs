use super::*;

#[test]
fn detect_verify_commands_probes_repo_and_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // repo probing: cargo + make (test preferred over test-all) + npm
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(root.join("Makefile"), "test-all:\n\tt\ntest:\n\tt\n").unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"scripts": {"test": "jest"}}"#,
    )
    .unwrap();
    let found = Config::detect_verify_commands(root);
    let get = |n: &str| found.iter().find(|(k, _, _)| k == n).cloned();
    // first source wins the name; cargo probed before make/npm
    assert_eq!(
        get("test").map(|(_, c, _)| c),
        Some("cargo test".to_string())
    );
    // SQWAI.md overrides on clash + adds new names
    std::fs::write(
        root.join("SQWAI.md"),
        "notes\nverify: test = make test-ci\nverify: lint = cargo clippy\n",
    )
    .unwrap();
    let found = Config::detect_verify_commands(root);
    let get = |n: &str| found.iter().find(|(k, _, _)| k == n).cloned();
    assert_eq!(
        get("test"),
        Some(("test".to_string(), "make test-ci".to_string(), "SQWAI.md"))
    );
    assert_eq!(
        get("lint").map(|(_, c, _)| c),
        Some("cargo clippy".to_string())
    );
    // nothing anywhere: empty, not an error
    let empty = tempfile::tempdir().unwrap();
    assert!(Config::detect_verify_commands(empty.path()).is_empty());
}

#[test]
fn seed_verify_commands_merges_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    // pre-existing hand-written values survive; unrelated content stays
    std::fs::create_dir_all(root.join(".sqwai")).unwrap();
    std::fs::write(
        root.join(".sqwai/config.toml"),
        "[plan]\nmax_steps = 9\n[verify]\ncommands = { test = \"hand command\" }\n",
    )
    .unwrap();
    let (added, already) = Config::seed_verify_commands(root);
    assert!(added.is_empty(), "hand value must win: {added:?}");
    assert_eq!(already, 1);
    // hostile shape: no panic, no write clobber
    std::fs::write(root.join(".sqwai/config.toml"), "verify = 5\n").unwrap();
    let (added, _) = Config::seed_verify_commands(root);
    assert!(added.is_empty());
    // missing file: dir created, map written, readable back
    let fresh = tempfile::tempdir().unwrap();
    std::fs::write(fresh.path().join("Cargo.toml"), "[package]\n").unwrap();
    let (added, _) = Config::seed_verify_commands(fresh.path());
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].0, "test");
    let back = Config::project_verify_commands(fresh.path());
    assert_eq!(back.get("test").map(String::as_str), Some("cargo test"));
}

#[test]
fn project_overrides_accept_verify_commands() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".sqwai")).unwrap();
    std::fs::write(
        dir.path().join(".sqwai/config.toml"),
        "[verify]\ncommands = { unit = \"cargo test --lib\" }\n",
    )
    .unwrap();
    // parsed through the same ProjectOverrides path as real files
    let raw = std::fs::read_to_string(dir.path().join(".sqwai/config.toml")).unwrap();
    let o: ProjectOverrides = toml::from_str(&raw).unwrap();
    let cmds = o.verify.commands.expect("verify.commands parses");
    assert_eq!(
        cmds.get("unit").map(String::as_str),
        Some("cargo test --lib")
    );
    // and the allowlist admits the section (no ignoring note)
    let mut cfg = Config::default();
    let notes = cfg.apply_project_overrides(dir.path());
    assert!(
        !notes.iter().any(|n| n.contains("verify")),
        "verify must be allowlisted: {notes:?}"
    );
    assert_eq!(
        cfg.verify.commands.get("unit").map(String::as_str),
        Some("cargo test --lib")
    );
}

#[test]
fn default_config_template_has_resolvable_last_model() {
    let cfg = Config::default();
    assert!(!cfg.last_model.is_empty());
    assert!(cfg.last_model_config().is_ok());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    write_template(&path).unwrap();
    let mut loaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    loaded.apply_builtins();
    loaded.migrate_last_model();
    assert!(loaded.last_model_config().is_ok());
}

#[test]
fn legacy_default_model_migrates_to_last_model_once() {
    let mut cfg: Config = toml::from_str(
        r#"
default_model = "my-old"
[models."my-old"]
provider = "p"
id = "my-old"
context = 1000
effort = "off"
"#,
    )
    .unwrap();
    assert_eq!(cfg.last_model, "");
    assert_eq!(cfg.legacy_default_model, "my-old");
    cfg.fold_models();
    cfg.apply_builtins();
    cfg.migrate_last_model();
    // the stored bare name resolves to the nested identity of its provider
    assert_eq!(cfg.last_model, "p/my-old");
    assert!(cfg.models.contains_key("p/my-old"));
    assert!(cfg.legacy_default_model.is_empty());

    // dangling reference (model deleted since) falls back, not errors
    cfg.last_model = "gone".into();
    cfg.migrate_last_model();
    assert_eq!(cfg.last_model, first_builtin_model());
    assert!(cfg.last_model_config().is_ok());
}

/// The config example in the README must actually load.
///
/// It did not: it used a `preset = "..."` key that does not exist and
/// omitted `format` and `base_url`, which are required, so anyone who
/// copied it got a parse error instead of an agent. Documentation that
/// claims to be runnable should be run.
#[test]
fn readme_config_example_parses() {
    let readme =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
            .expect("README.md");
    let block = readme
        .split("```toml")
        .nth(1)
        .and_then(|rest| rest.split("```").next());
    // no TOML example in the README (by design — config is
    // self-documented): nothing runnable to verify
    let Some(block) = block else {
        return;
    };
    let cfg: Config = toml::from_str(block)
        .unwrap_or_else(|e| panic!("README config example does not parse: {e}\n{block}"));
    assert!(
        cfg.providers.values().all(|p| !p.base_url.is_empty()),
        "every provider in the example needs a base_url"
    );
    assert!(
        cfg.models.contains_key(&cfg.last_model),
        "last_model {:?} must be one of the example's models",
        cfg.last_model
    );
}

#[test]
fn effort_accepts_string_and_legacy_bool() {
    #[derive(Deserialize)]
    struct T {
        effort: EffortLevel,
    }
    let t: T = toml::from_str("effort = false").unwrap();
    assert_eq!(t.effort, EffortLevel::Off);
    let t: T = toml::from_str("effort = true").unwrap();
    assert_eq!(t.effort, EffortLevel::High);
    let t: T = toml::from_str("effort = \"max\"").unwrap();
    assert_eq!(t.effort, EffortLevel::Max);
}

#[test]
fn effort_strs_cover_every_level_in_order() {
    // pick-one UI builds on this: a hardcoded copy once dropped xhigh
    let from_all: Vec<&str> = EffortLevel::ALL.iter().map(|l| l.as_str()).collect();
    assert_eq!(&EffortLevel::STRS[..], &from_all[..]);
}

#[test]
fn effort_control_strs_cover_every_variant_in_order() {
    let from_all: Vec<&str> = EffortControl::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(&EffortControl::STRS[..], &from_all[..]);
    assert_eq!(EffortControl::from_str("named"), Some(EffortControl::Named));
    assert_eq!(EffortControl::from_str("auto"), None);
}

/// The declaration is an override, not a requirement: a model that says
/// nothing gets the conservative reading of its wire format.
#[test]
fn effort_support_falls_back_to_the_wire_format() {
    let mut m: ModelConfig = toml::from_str(
        "provider = \"anthropic\"\nid = \"claude-sonnet-5\"\ncontext = 1000\neffort = \"high\"\n",
    )
    .unwrap();
    assert_eq!(m.effort_control, None);
    assert_eq!(
        m.effort_support(WireFormat::Anthropic).control,
        EffortControl::Budget,
        "the Messages API expresses effort as a token budget"
    );
    assert_eq!(
        m.effort_support(WireFormat::Openai).control,
        EffortControl::Named,
        "an unknown OpenAI-compatible endpoint gets literal level names"
    );
    assert!(!m.effort_support(WireFormat::Openai).always_on);

    m.effort_control = Some(EffortControl::Named);
    assert_eq!(
        m.effort_support(WireFormat::Openai).control,
        EffortControl::Named,
        "a documented named-level model must be able to say so"
    );
}

/// A declaration the wire format cannot express would otherwise make the
/// request carry nothing while the UI called the level applied.
#[test]
fn a_declaration_is_clamped_to_what_the_format_can_express() {
    let mut m: ModelConfig = toml::from_str(
        "provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"high\"\neffort_control = \"levels\"\n",
    )
    .unwrap();
    assert_eq!(
        m.effort_support(WireFormat::Anthropic).control,
        EffortControl::Budget
    );
    m.effort_control = Some(EffortControl::Budget);
    assert_eq!(
        m.effort_support(WireFormat::Openai).control,
        EffortControl::Named
    );
    // shapes the format *can* express are left alone
    m.effort_control = Some(EffortControl::Toggle);
    assert_eq!(
        m.effort_support(WireFormat::Openai).control,
        EffortControl::Toggle
    );
}

#[test]
fn effort_declaration_parses_and_round_trips() {
    let m: ModelConfig = toml::from_str(
        "provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"max\"\n\
         effort_control = \"named\"\neffort_always_on = true\n",
    )
    .unwrap();
    assert_eq!(m.effort_control, Some(EffortControl::Named));
    assert!(m.effort_always_on);
    let back: ModelConfig = toml::from_str(&toml::to_string_pretty(&m).unwrap()).unwrap();
    assert_eq!(back.effort_control, m.effort_control);
    assert!(back.effort_always_on);

    // legacy spellings still parse to the same control
    for legacy in ["levels", "xhigh"] {
        let old: ModelConfig = toml::from_str(&format!(
            "provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"high\"\neffort_control = \"{legacy}\"\n"
        ))
        .unwrap();
        assert_eq!(old.effort_control, Some(EffortControl::Named), "{legacy}");
    }

    // a model that declares nothing writes nothing
    let plain: ModelConfig =
        toml::from_str("provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"low\"\n").unwrap();
    let text = toml::to_string_pretty(&plain).unwrap();
    assert!(!text.contains("effort_control"), "{text}");
    assert!(!text.contains("effort_always_on"), "{text}");
}

/// configs written before the slider was renamed say `thinking`; they must
/// keep loading, including the `thinking = true/false` spelling that
/// predates the levels.
#[test]
fn legacy_thinking_keys_still_load() {
    let m: ModelConfig = toml::from_str(
        "provider = \"anthropic\"\nid = \"claude-sonnet-5\"\ncontext = 1000000\nthinking = \"max\"\n",
    )
    .unwrap();
    assert_eq!(m.effort, EffortLevel::Max);
    let m: ModelConfig = toml::from_str(
        "provider = \"openai\"\nid = \"gpt-5.6\"\ncontext = 922000\nthinking = true\n",
    )
    .unwrap();
    assert_eq!(m.effort, EffortLevel::High);
}

/// A provider that says nothing about continuation gets the safe answer. The
/// field used to default to `true` and be read by nobody, which promised a
/// saving and delivered none — and a host that accepts `previous_response_id`
/// while storing nothing would have been trusted by default the day it was
/// wired.
#[test]
fn continuation_defaults_to_off_and_is_only_what_the_owner_declares() {
    let quiet: ProviderConfig = toml::from_str(
        "format = \"responses\"
base_url = \"https://api.pateway.ai/v1\"
",
    )
    .unwrap();
    assert!(!quiet.continuation, "no declaration means no chain");
    let declared: ProviderConfig = toml::from_str(
        "format = \"responses\"
base_url = \"https://api.openai.com/v1\"
continuation = true
",
    )
    .unwrap();
    assert!(
        declared.continuation,
        "an owner who checked the endpoint can turn it on"
    );
}
#[test]
fn config_roundtrip() {
    let cfg = Config::default();
    let s = toml::to_string_pretty(&cfg).unwrap();
    let back: Config = toml::from_str(&s).unwrap();
    assert_eq!(back.providers.len(), cfg.providers.len());
}

#[test]
fn conventional_env_name_uppercases_provider() {
    assert_eq!(Config::conventional_env_name("openai"), "OPENAI_API_KEY");
    assert_eq!(
        Config::conventional_env_name("open-router"),
        "OPEN_ROUTER_API_KEY"
    );
    assert_eq!(Config::conventional_env_name("zen"), "ZEN_API_KEY");
}

#[test]
fn api_key_resolves_from_env() {
    unsafe { std::env::set_var("SQWAI_TEST_PROVIDER_KEY", "explicit") };
    unsafe { std::env::set_var("SQWAI_TEST_CONV_API_KEY", "implicit") };
    let mk = |api_key: Option<&str>, api_key_env: Option<&str>| ProviderConfig {
        format: WireFormat::Openai,
        base_url: "http://x".into(),
        api_key: api_key.map(str::to_string),
        api_key_env: api_key_env.map(str::to_string),
        continuation: true,
        models: Default::default(),
    };
    // explicit env variable
    assert_eq!(
        mk(None, Some("SQWAI_TEST_PROVIDER_KEY")).effective_api_key("p"),
        Some("explicit".into())
    );
    // implicit <PROVIDER>_API_KEY convention
    assert_eq!(
        mk(None, None).effective_api_key("sqwai_test_conv"),
        Some("implicit".into())
    );
    // inline key wins over the environment
    assert_eq!(
        mk(Some("inline"), Some("SQWAI_TEST_PROVIDER_KEY")).effective_api_key("p"),
        Some("inline".into())
    );
}

#[test]
fn builtins_merge_without_overwriting_user_edits() {
    let mut cfg = Config::default();
    let expected_providers = [
        "gemini",
        "anthropic",
        "openai",
        "deepseek",
        "grok",
        "kimi",
        "alibaba",
        "zai",
        "minimax",
        "mistral",
        "cohere",
        "groq",
        "cerebras",
        "togetherai",
        "fireworks",
        "deepinfra",
        "nvidia",
        "openrouter",
    ];
    for p in expected_providers {
        assert!(cfg.providers.contains_key(p), "missing provider {p}");
        assert!(cfg.is_builtin_provider(p), "not marked builtin {p}");
    }
    // Exact model ids float with the catalog, so assert structurally:
    // every builtin provider contributes builtin models.
    for p in expected_providers {
        assert!(
            cfg.models
                .iter()
                .any(|(k, m)| m.provider == p && cfg.is_builtin_model(k)),
            "provider {p} has no builtin models"
        );
    }
    // auto-discovered providers resolve at least one model each; exact
    // revisions float with the catalog, so assert structurally, not by id
    for (provider, min_models) in [("anthropic", 5), ("openai", 5), ("grok", 4), ("kimi", 3)] {
        let count = cfg
            .models
            .values()
            .filter(|m| m.provider == provider)
            .count();
        assert!(
            count >= min_models,
            "provider {provider} has only {count} models"
        );
    }
    // retired IDs must be gone, not kept alongside. The list is a
    // snapshot of delistings, not doctrine: if a provider serves an id
    // again (gpt-4o rejoined via direct discovery 2026-09-27), it leaves
    // this list — presence in the live API wins over history.
    for retired in [
        "claude-3-7-sonnet-20250219",
        "o1-mini",
        "grok-2-1212",
        "grok-beta",
        "moonshot-v1-8k",
        "kimi-latest",
        "deepseek-chat",
    ] {
        assert!(
            !cfg.is_builtin_model(retired),
            "retired model {retired} still builtin"
        );
    }

    // user-defined model survives apply_builtins
    cfg.models.insert(
        "my-custom-model".into(),
        ModelConfig {
            provider: "custom".into(),
            id: "my-id".into(),
            context: 128000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
            status: ModelStatus::Active,
            vision: None,
            pdf: None,
        },
    );
    cfg.apply_builtins();
    assert!(cfg.models.contains_key("my-custom-model"));
    assert!(!cfg.is_builtin_model("my-custom-model"));
}

/// A fetched catalog is applied to provider endpoints with the user's
/// key following them, so it validates like a trust boundary: https on
/// a public host, no inline secrets, pinned origins for builtins,
/// serial never backwards.
#[test]
fn fetched_catalog_validates_base_urls_secrets_and_freshness() {
    fn catalog(base_url: &str, serial: u64, api_key: bool) -> String {
        format!(
            "serial = {serial}\n[providers.acme]\nformat = \"openai\"\nbase_url = \"{base_url}\"{}\n[models.\"acme-x\"]\nprovider = \"acme\"\nid = \"acme-x\"\ncontext = 1000\neffort = \"medium\"\n",
            if api_key {
                "\napi_key = \"sk-planted\""
            } else {
                ""
            },
        )
    }
    let good = catalog("https://api.acme.example/v1", 10, false);
    let parsed = validate_fetched_catalog(&good, 9).unwrap();
    assert!(parsed.providers.contains_key("acme"));
    // same serial is a no-op upstream, not a rollback
    assert!(validate_fetched_catalog(&good, 10).is_ok());
    // the shipped fallback itself passes (the gate must never reject
    // the file it is meant to protect the updates of)
    assert!(validate_fetched_catalog(BUILTIN_PROVIDERS_FALLBACK, 0).is_ok());

    for (case, url, serial, key) in [
        ("plain http", "http://api.acme.example/v1", 10, false),
        ("loopback https", "https://127.0.0.1/v1", 10, false),
        ("private https", "https://10.0.0.5/v1", 10, false),
        ("localhost", "https://localhost:8443/v1", 10, false),
        ("inline secret", "https://api.acme.example/v1", 10, true),
        ("stale serial", "https://api.acme.example/v1", 8, false),
    ] {
        assert!(
            validate_fetched_catalog(&catalog(url, serial, key), 9).is_err(),
            "fetched catalog accepted {case}"
        );
    }
}

/// Pinned origins: a builtin provider that moves host or rebinds its
/// key env fails the whole file (that is key exfiltration, not an
/// update). Unknown providers pass validation but are skipped at apply.
#[test]
fn fetched_catalog_pins_builtin_origins() {
    fn catalog(name: &str, base_url: &str, key_env: &str) -> String {
        format!(
            "serial = 10\n[providers.{name}]\nformat = \"openai\"\nbase_url = \"{base_url}\"\napi_key_env = \"{key_env}\"\n"
        )
    }
    // moved host refuses (public-looking, so only the pin catches it)
    let err = validate_fetched_catalog(
        &catalog(
            "openai",
            "https://api.evil-example.com/v1",
            "OPENAI_API_KEY",
        ),
        9,
    )
    .unwrap_err();
    assert!(err.to_string().contains("pinned"), "{err:?}");
    // rebound key env refuses
    let err = validate_fetched_catalog(
        &catalog("openai", "https://api.openai.com/v1", "EVIL_KEY"),
        9,
    )
    .unwrap_err();
    assert!(err.to_string().contains("pinned"), "{err:?}");
    // pinned values pass
    assert!(
        validate_fetched_catalog(
            &catalog("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
            9
        )
        .is_ok()
    );
    // unknown providers pass validation (skipped at apply, never added)
    assert!(
        validate_fetched_catalog(
            &catalog("acme", "https://api.acme.example/v1", "ACME_API_KEY"),
            9
        )
        .is_ok()
    );
}

/// Audit C8: the disk path used to `toml::from_str` whatever sat in
/// `builtin_providers.toml` — validation ran on the network fetch
/// only, so a cache rewritten behind the app's back went straight
/// into `apply_builtins`, base_url and the user's key with it.
#[test]
fn disk_catalog_runs_the_same_gate_as_a_fetch() {
    fn catalog(base_url: &str, serial: u64, api_key: bool) -> String {
        format!(
            "serial = {serial}\n[providers.acme]\nformat = \"openai\"\nbase_url = \"{base_url}\"{}\n",
            if api_key {
                "\napi_key = \"sk-planted\""
            } else {
                ""
            },
        )
    }
    let fallback: BuiltinCatalog = toml::from_str(BUILTIN_PROVIDERS_FALLBACK).unwrap();
    // a legitimate fresh catalog passes
    assert!(
        validated_disk_catalog(
            &catalog("https://api.acme.example/v1", 99, false),
            &fallback
        )
        .is_ok()
    );
    for (case, url, serial, key) in [
        ("plain http", "http://attacker.example/v1", 99, false),
        ("loopback https", "https://127.0.0.1/v1", 99, false),
        ("inline secret", "https://api.acme.example/v1", 99, true),
    ] {
        assert!(
            validated_disk_catalog(&catalog(url, serial, key), &fallback).is_err(),
            "disk cache accepted {case}"
        );
    }
    // rollback below the shipped floor refuses even with good urls
    let shipped: BuiltinCatalog =
        toml::from_str("serial = 50\n[providers.acme]\nformat = \"openai\"\nbase_url = \"https://api.acme.example/v1\"\n").unwrap();
    assert!(
        validated_disk_catalog(&catalog("https://api.acme.example/v1", 49, false), &shipped)
            .is_err()
    );
    assert!(
        validated_disk_catalog(&catalog("https://api.acme.example/v1", 50, false), &shipped)
            .is_ok()
    );
}

#[test]
fn customized_builtin_model_survives_save_strip_and_reapply() {
    // the hand-edit path: user overrides one field of a built-in model
    let mut cfg = Config::default();
    cfg.apply_builtins();
    let key = "gemini/gemini-3.8-flash";
    assert!(cfg.is_builtin_model(key));
    let mut customized = cfg.models[key].clone();
    customized.effort = EffortLevel::High;
    cfg.models.insert(key.into(), customized.clone());

    // save-strip keeps the customized entry, drops pristine ones
    let stripped = cfg.without_pristine_builtins();
    assert_eq!(
        stripped.models.get(key),
        Some(&customized),
        "customized built-in must persist"
    );
    for k in stripped.models.keys() {
        if k != key {
            assert!(
                !BuiltinCatalog::current().models.contains_key(k),
                "pristine built-in {k} leaked into the user file"
            );
        }
    }

    // reload: user entry wins over the catalog, untouched keys refresh
    let mut reloaded = stripped;
    reloaded.apply_builtins();
    assert_eq!(
        reloaded.models.get(key),
        Some(&customized),
        "re-apply must not clobber the user override"
    );
}

#[test]
fn catalog_transition_updates_pristine_specs_and_drops_removed_builtins() {
    let old_cat = BuiltinCatalog::from_toml_str(
        "serial = 1\n\
         [providers.openai]\nformat = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\napi_key_env = \"OPENAI_API_KEY\"\n\
         [models.\"m-keep\"]\nprovider = \"openai\"\nid = \"m-keep\"\ncontext = 128000\neffort = \"medium\"\n\
         [models.\"m-custom\"]\nprovider = \"openai\"\nid = \"m-custom\"\ncontext = 128000\neffort = \"medium\"\n\
         [models.\"m-retired\"]\nprovider = \"openai\"\nid = \"m-retired\"\ncontext = 16385\neffort = \"off\"\n",
    )
    .unwrap();
    let new_cat = BuiltinCatalog::from_toml_str(
        "serial = 2\n\
         [providers.openai]\nformat = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\napi_key_env = \"OPENAI_API_KEY\"\n\
         [models.\"m-keep\"]\nprovider = \"openai\"\nid = \"m-keep\"\ncontext = 400000\neffort = \"high\"\n\
         [models.\"m-custom\"]\nprovider = \"openai\"\nid = \"m-custom\"\ncontext = 400000\neffort = \"high\"\n",
    )
    .unwrap();

    let mut cfg = Config {
        last_model: "openai/m-keep".into(),
        legacy_default_model: String::new(),
        providers: old_cat.providers.clone(),
        models: old_cat.models.clone(),
        ..Config::default()
    };
    // User customized `m-custom` effort to Low
    cfg.models.get_mut("openai/m-custom").unwrap().effort = EffortLevel::Low;

    cfg.apply_catalog_transition(&old_cat, &new_cat);

    // Pristine `m-keep` updated its context and effort to `new_cat`
    assert_eq!(cfg.models["openai/m-keep"].context, 400000);
    assert_eq!(cfg.models["openai/m-keep"].effort, EffortLevel::High);

    // User-customized `m-custom` kept its user override
    assert_eq!(cfg.models["openai/m-custom"].context, 128000);
    assert_eq!(cfg.models["openai/m-custom"].effort, EffortLevel::Low);

    // Pristine `m-retired` was removed and does not linger
    assert!(!cfg.models.contains_key("openai/m-retired"));
}

#[test]
fn project_overrides_apply_allowlisted_keys_and_report_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let sqwai = dir.path().join(".sqwai");
    std::fs::create_dir_all(&sqwai).unwrap();
    std::fs::write(
        sqwai.join("config.toml"),
        r#"
[plan]
max_steps = 5
nudge_after = 7
plan_first = "off"

[safety]
blocked_patterns = ["rm -rf /"]

[web]
allow_hosts = ["127.0.0.1:3000"]

[models."x"]
provider = "p"
"#,
    )
    .unwrap();

    let mut cfg = Config::default();
    let notes = cfg.apply_project_overrides(dir.path());
    assert_eq!(cfg.plan.max_steps, 5);
    assert_eq!(cfg.plan.nudge_after, 7);
    // not allowlisted: reported, never applied
    assert!(
        notes.iter().any(|n| n.contains("plan.plan_first")),
        "{notes:?}"
    );
    assert!(notes.iter().any(|n| n.contains("[safety]")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("[models]")), "{notes:?}");
    // a cloned repo must not widen the hosts our webfetch may reach
    assert!(notes.iter().any(|n| n.contains("[web]")), "{notes:?}");
    assert!(
        cfg.web.allow_hosts.is_empty(),
        "project file must not open loopback targets: {:?}",
        cfg.web.allow_hosts
    );
    assert!(
        !cfg.models.contains_key("x"),
        "project file must not inject models"
    );
}

#[test]
fn project_overrides_missing_file_is_silent_and_broken_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    assert!(cfg.apply_project_overrides(dir.path()).is_empty());

    let sqwai = dir.path().join(".sqwai");
    std::fs::create_dir_all(&sqwai).unwrap();
    std::fs::write(sqwai.join("config.toml"), "[plan\nmax_steps = ").unwrap();
    let before = cfg.plan.max_steps;
    let notes = cfg.apply_project_overrides(dir.path());
    assert_eq!(
        cfg.plan.max_steps, before,
        "broken file must change nothing"
    );
    assert!(!notes.is_empty(), "broken file must be reported");
}

#[test]
fn test_model_config_fallback_deserialization() {
    let toml_str = r#"
        [models.primary]
        provider = "openai"
        id = "gpt-4o"
        context = 128000
        effort = "off"
        fallback = "secondary"

        [models.secondary]
        provider = "anthropic"
        id = "claude-3-5-sonnet"
        context = 200000
        effort = "off"
    "#;
    let mut cfg: Config = toml::from_str(toml_str).unwrap();
    cfg.fold_models();
    assert_eq!(
        cfg.models["openai/primary"].fallback.as_deref(),
        Some("secondary")
    );
    assert_eq!(cfg.models["anthropic/secondary"].fallback, None);
}

#[test]
fn test_resolve_fallback_chain_handles_order_and_cycles() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "p1".into(),
        ProviderConfig {
            format: WireFormat::Openai,
            base_url: "http://localhost:1".into(),
            api_key: Some("dummy".into()),
            api_key_env: None,
            continuation: true,
            models: Default::default(),
        },
    );
    cfg.providers.insert(
        "p2".into(),
        ProviderConfig {
            format: WireFormat::Anthropic,
            base_url: "http://localhost:2".into(),
            api_key: Some("dummy".into()),
            api_key_env: None,
            continuation: true,
            models: Default::default(),
        },
    );

    // A -> B -> C -> A (cycle)
    cfg.models.insert(
        "m_a".into(),
        ModelConfig {
            provider: "p1".into(),
            id: "id_a".into(),
            context: 1000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: Some("m_b".into()),
            status: ModelStatus::Active,
            vision: None,
            pdf: None,
        },
    );
    cfg.models.insert(
        "m_b".into(),
        ModelConfig {
            provider: "p2".into(),
            id: "id_b".into(),
            context: 2000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: Some("m_c".into()),
            status: ModelStatus::Active,
            vision: None,
            pdf: None,
        },
    );
    cfg.models.insert(
        "m_c".into(),
        ModelConfig {
            provider: "p1".into(),
            id: "id_c".into(),
            context: 3000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: Some("m_a".into()), // cycle back to A
            status: ModelStatus::Active,
            vision: None,
            pdf: None,
        },
    );

    let chain = cfg.resolve_fallback_chain("m_a");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].0, "m_b");
    assert_eq!(chain[0].1.id, "id_b");
    assert_eq!(chain[1].0, "m_c");
    assert_eq!(chain[1].1.id, "id_c");
    // m_c fallback to m_a was not added due to cycle protection
}

/// The opt-in has to survive a real config file, not just a struct
/// literal: an empty default must mean "shut", and a written list must
/// arrive at the gate unchanged.
#[test]
fn web_allow_hosts_parse_from_toml() {
    let empty: Config = toml::from_str("").unwrap();
    assert!(
        empty.web.allow_hosts.is_empty(),
        "default must stay shut: {:?}",
        empty.web.allow_hosts
    );
    let cfg: Config =
        toml::from_str("[web]\nallow_hosts = [\"localhost:3000\", \"127.0.0.1\"]\n").unwrap();
    assert_eq!(
        cfg.web.allow_hosts,
        vec!["localhost:3000".to_string(), "127.0.0.1".to_string()]
    );
    // and it survives a save/load round trip
    let text = toml::to_string(&cfg).unwrap();
    let again: Config = toml::from_str(&text).unwrap();
    assert_eq!(again.web.allow_hosts, cfg.web.allow_hosts);
}

fn gate(url: &str, allow: &[&str]) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).expect("test url parses");
    url_host_allowed(
        &parsed,
        &allow.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
}

/// the opt-in opens exactly the loopback class, and only when the
/// user named it. Empty list = today's behavior, refuse every local host.
#[test]
fn loopback_needs_an_explicit_entry() {
    for local in [
        "http://localhost:3000/api",
        "http://my.app.localhost:8080/",
        "http://127.0.0.1:11434/v1",
        "http://[::1]:8080/",
    ] {
        assert!(
            gate(local, &[]).is_err(),
            "{local} must stay shut by default"
        );
    }
    assert!(gate("http://localhost:3000/api", &["localhost:3000"]).is_ok());
    assert!(gate("http://localhost:9999/api", &["localhost:3000"]).is_err());
    // a portless entry covers every port of that host
    assert!(gate("http://127.0.0.1:11434/v1", &["127.0.0.1"]).is_ok());
    assert!(gate("http://[::1]:8080/", &["[::1]:8080"]).is_ok());
    assert!(gate("http://[::1]:8080/", &["[::1]:9"]).is_err());
    // the scheme default counts as the port
    assert!(gate("http://my.app.localhost/app", &["my.app.localhost:80"]).is_ok());
}

/// Everything else in the non-public space stays refused with the host
/// listed: the metadata names are the IMDS target, the other ranges are
/// the SSRF blast radius a dev-server allowance must not open.
#[test]
fn allowlist_never_opens_metadata_or_internal_ranges() {
    for (url, listed) in [
        (
            "http://169.254.169.254/latest/meta-data/",
            "169.254.169.254",
        ),
        ("http://10.0.0.5/admin", "10.0.0.5"),
        ("http://192.168.1.1/", "192.168.1.1"),
        ("http://172.16.0.1/", "172.16.0.1"),
        (
            "http://metadata.google.internal/",
            "metadata.google.internal",
        ),
        ("http://0.0.0.0/", "0.0.0.0"),
    ] {
        assert!(gate(url, &[]).is_err(), "{url} must be refused");
        assert!(
            gate(url, &[listed]).is_err(),
            "{url} must stay refused when listed"
        );
    }
    // WHATWG numeric spellings of loopback normalize to a dotted quad, so
    // they land in the loopback class and follow the list
    assert!(gate("http://2130706433:3000/", &[]).is_err());
    assert!(gate("http://2130706433:3000/", &["127.0.0.1:3000"]).is_ok());
}

/// Public targets never consult the list, and an entry is never a
/// wildcard: matching is by host (plus optional port), exact.
#[test]
fn public_hosts_ignore_the_list() {
    assert!(gate("https://example.com/", &[]).is_ok());
    assert!(gate("https://example.com/", &["localhost"]).is_ok());
    assert!(gate("https://8.8.8.8/", &[]).is_ok());
    assert!(gate("http://example.com./", &[]).is_ok());
}

#[test]
fn tool_and_turn_limit_defaults_preserve_the_old_constants() {
    let cfg = Config::default();
    assert_eq!(cfg.tools.bash.timeout_secs, 120);
    assert_eq!(cfg.tools.bash.output_bytes, 30_000);
    assert_eq!(cfg.tools.bash.background_jobs, 8);
    assert_eq!(cfg.tools.read.window, 2_000);
    assert_eq!(cfg.tools.read.max_file_bytes, 16 * 1024 * 1024);
    assert_eq!(cfg.tools.outline.max_file_bytes, 2_097_152);
    assert_eq!(cfg.tools.web.results, 6);
    assert_eq!(cfg.tools.web.timeout_secs, 15);
    assert_eq!(cfg.tools.web.fetch_bytes, 1_500_000);
    assert_eq!(cfg.limits.max_tool_calls_per_turn, 0);
    assert_eq!(cfg.limits.max_tokens_per_turn, 0);
    assert_eq!(cfg.limits.max_parallel_tools, 24);
    assert!(cfg.ui.open_attachments);
    assert!(cfg.ui.mouse);
}

#[test]
fn tools_limits_and_ui_toggles_parse_from_the_user_file() {
    let cfg: Config = toml::from_str(
        r#"
[tools.bash]
timeout_secs = 30
output_bytes = 9000
background_jobs = 2

[tools.read]
window = 120
max_file_bytes = 4096

[tools.web]
results = 3
timeout_secs = 5
fetch_bytes = 65536

[limits]
max_tool_calls_per_turn = 12
max_tokens_per_turn = 50000
max_parallel_tools = 2

[ui]
open_attachments = false
mouse = false
"#,
    )
    .unwrap();
    assert_eq!(cfg.tools.bash.timeout_secs, 30);
    assert_eq!(cfg.tools.bash.background_jobs, 2);
    assert_eq!(cfg.tools.read.window, 120);
    assert_eq!(cfg.tools.web.results, 3);
    assert_eq!(cfg.limits.max_tool_calls_per_turn, 12);
    assert_eq!(cfg.limits.max_tokens_per_turn, 50_000);
    assert_eq!(cfg.limits.max_parallel_tools, 2);
    assert!(!cfg.ui.open_attachments);
    assert!(!cfg.ui.mouse);
}

#[test]
fn approval_mode_parses_and_defaults() {
    // default: auto (the behavior the app always had)
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.safety.approval_mode, ApprovalMode::Auto);
    // each documented spelling
    for (text, want) in [
        ("ask", ApprovalMode::Ask),
        ("auto", ApprovalMode::Auto),
        ("full", ApprovalMode::Full),
    ] {
        let cfg: Config = toml::from_str(&format!(
            "[safety]
approval_mode = \"{text}\"
"
        ))
        .unwrap();
        assert_eq!(cfg.safety.approval_mode, want, "approval_mode = {text}");
    }
    // unknown value refuses to load rather than silently downgrading
    assert!(
        toml::from_str::<Config>(
            "[safety]
approval_mode = \"yolo\"
"
        )
        .is_err()
    );
}

#[test]
fn approval_mode_cycle_is_the_escalation_ladder() {
    assert_eq!(ApprovalMode::Ask.cycle_next(), ApprovalMode::Auto);
    assert_eq!(ApprovalMode::Auto.cycle_next(), ApprovalMode::Full);
    assert_eq!(ApprovalMode::Full.cycle_next(), ApprovalMode::Ask);
    // the flag accepts the longer spelling humans type
    assert_eq!(
        ApprovalMode::from_str("full_access"),
        Some(ApprovalMode::Full)
    );
    assert_eq!(ApprovalMode::from_str("nope"), None);
}

// ——— nested model identity ———

fn model(provider: &str, id: &str, context: u64) -> ModelConfig {
    ModelConfig {
        provider: provider.into(),
        id: id.into(),
        context,
        effort: EffortLevel::Medium,
        effort_control: None,
        effort_always_on: false,
        fallback: None,
        status: ModelStatus::Active,
        vision: None,
        pdf: None,
    }
}

/// The reason the nesting exists: one name served by two endpoints is two
/// models, not a collision to dodge with a fake key.
#[test]
fn nested_models_fold_under_their_provider_key() {
    let mut cfg: Config = toml::from_str(
        r#"
[providers.pateway]
format = "openai"
base_url = "https://a.example/v1"
[providers.pateway.models."gpt-6-luna"]
id = "gpt-6-luna"
context = 262144
effort = "high"

[providers.openrouter]
format = "openai"
base_url = "https://b.example/v1"
[providers.openrouter.models."gpt-6-luna"]
id = "openrouter/gpt-6"
context = 131072
effort = "low"
"#,
    )
    .unwrap();
    cfg.fold_models();
    assert_eq!(cfg.models["pateway/gpt-6-luna"].context, 262144);
    assert_eq!(cfg.models["openrouter/gpt-6-luna"].context, 131072);
    // the provider half is recovered from the table, not typed per model
    assert_eq!(cfg.models["openrouter/gpt-6-luna"].provider, "openrouter");
    assert_eq!(model_display("pateway/gpt-6-luna"), "gpt-6-luna @ pateway");
    // a model name may itself carry slashes: only the first segment is the provider
    assert_eq!(key_provider("deepinfra/zai-org/GLM-5.3"), Some("deepinfra"));
    assert_eq!(key_name("deepinfra/zai-org/GLM-5.3"), "zai-org/GLM-5.3");
}

#[test]
fn legacy_flat_models_fold_by_their_provider_field() {
    let mut cfg: Config = toml::from_str(
        r#"
[models."luna"]
provider = "pateway"
id = "luna"
context = 1000
effort = "off"

[providers.pateway]
format = "openai"
base_url = "https://a.example/v1"
[providers.pateway.models."luna"]
id = "luna"
context = 2000
effort = "off"
"#,
    )
    .unwrap();
    cfg.fold_models();
    // both halves of a half-migrated file land on the same key, and the table
    // the editor writes wins over the section it replaced
    assert_eq!(cfg.models.len(), 1);
    assert_eq!(cfg.models["pateway/luna"].context, 2000);

    // a flat model naming no provider keeps its bare key: unusable, but not lost
    let mut orphan: Config = toml::from_str(
        r#"
[models."x"]
id = "x"
context = 1000
effort = "off"
"#,
    )
    .unwrap();
    orphan.fold_models();
    assert!(orphan.models.contains_key("x"));
}

#[test]
fn bare_model_names_resolve_only_when_unambiguous() {
    let mut cfg = Config::default();
    cfg.models.clear();
    cfg.models
        .insert("pateway/luna".into(), model("pateway", "luna", 1));
    cfg.models
        .insert("openrouter/luna".into(), model("openrouter", "luna", 2));
    cfg.models
        .insert("openrouter/gpt-6-luna".into(), model("openrouter", "gpt-6", 3));
    cfg.models.insert("solo".into(), model("pateway", "solo", 4));

    assert_eq!(
        cfg.resolve_model_key("pateway/luna"),
        Some("pateway/luna")
    );
    // unique name, stored before nesting
    assert_eq!(
        cfg.resolve_model_key("gpt-6-luna"),
        Some("openrouter/gpt-6-luna")
    );
    // a bare name two providers serve resolves to nothing rather than a guess
    assert_eq!(cfg.resolve_model_key("luna"), None);
    // an unqualified name that exists as-is wins without a second look
    assert_eq!(cfg.resolve_model_key("solo"), Some("solo"));
    assert_eq!(cfg.resolve_model_key("nope"), None);
    assert_eq!(cfg.resolve_model_key(""), None);
}

/// What `save` writes must read back as the same set of models — including
/// the one case that cannot nest: a model whose provider is not in the file.
#[test]
fn nested_save_shape_round_trips() {
    let mut cfg = Config::default();
    cfg.models.clear();
    cfg.providers.clear();
    cfg.models
        .insert("pateway/luna".into(), model("pateway", "luna", 1000));
    cfg.models
        .insert("openrouter/luna".into(), model("openrouter", "luna", 2000));
    cfg.models
        .insert("ghost/model".into(), model("ghost", "model", 3000));
    cfg.providers.insert(
        "pateway".into(),
        ProviderConfig {
            format: WireFormat::Openai,
            base_url: "https://a.example/v1".into(),
            api_key: None,
            api_key_env: None,
            continuation: false,
            models: Default::default(),
        },
    );
    cfg.providers.insert(
        "openrouter".into(),
        ProviderConfig {
            format: WireFormat::Openai,
            base_url: "https://b.example/v1".into(),
            api_key: None,
            api_key_env: None,
            continuation: false,
            models: Default::default(),
        },
    );

    let text = toml::to_string_pretty(&cfg.for_file()).unwrap();
    assert!(
        text.contains(r#"[providers.pateway.models.luna]"#),
        "model must nest under its provider:\n{text}"
    );
    // the ghost provider holds no table, so its model keeps the flat section
    assert!(text.contains(r#"[models."ghost/model"]"#), "orphan lost:\n{text}");

    let mut back: Config = toml::from_str(&text).unwrap();
    back.fold_models();
    assert_eq!(back.models, cfg.models, "round trip changed the models");
}

#[test]
fn provider_rename_moves_model_identity() {
    let mut cfg = Config::default();
    cfg.models.insert("old/luna".into(), model("old", "luna", 1));
    cfg.models.insert("old/other".into(), model("old", "other", 2));
    cfg.models.insert("new/other".into(), model("new", "other", 3));
    cfg.last_model = "old/luna".into();

    let skipped = cfg.rename_provider_models("old", "new");

    assert_eq!(skipped, 1, "the clashing model is reported, not merged");
    assert_eq!(cfg.models["new/luna"].provider, "new");
    assert_eq!(cfg.last_model, "new/luna");
    // the clash keeps both: nothing is deleted, the loser stays where it was
    assert_eq!(cfg.models["new/other"].context, 3);
    assert_eq!(cfg.models["old/other"].context, 2);
}

/// The catalog file stays flat (its generator writes it that way); what it
/// deserializes into is the same `provider/name` identity the user file nests.
#[test]
fn builtin_catalog_keys_are_qualified() {
    let catalog = BuiltinCatalog::from_toml_str(
        "serial = 1\n\
         [providers.zai]\nformat = \"openai\"\nbase_url = \"https://z.example/v1\"\n\
         [providers.deepinfra]\nformat = \"openai\"\nbase_url = \"https://d.example/v1\"\n\
         [models.\"glm-5.2\"]\nprovider = \"zai\"\nid = \"glm-5.2\"\ncontext = 100\neffort = \"off\"\n\
         [models.\"zai-org/GLM-5.3\"]\nprovider = \"deepinfra\"\nid = \"GLM-5.3\"\ncontext = 200\neffort = \"off\"\n",
    )
    .unwrap();
    assert!(catalog.models.contains_key("zai/glm-5.2"));
    assert!(catalog.models.contains_key("deepinfra/zai-org/GLM-5.3"));
    assert_eq!(catalog.models["zai/glm-5.2"].provider, "zai");
    // menu order follows the file, in qualified keys
    assert_eq!(
        catalog.model_order,
        vec!["zai/glm-5.2".to_string(), "deepinfra/zai-org/GLM-5.3".to_string()]
    );
    // the shipped catalog is qualified too, and no two of its keys collide
    let shipped = BuiltinCatalog::current();
    assert!(
        shipped.models.keys().all(|k| k.contains('/')),
        "catalog keys must name their provider"
    );
}

