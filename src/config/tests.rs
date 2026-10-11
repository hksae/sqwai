use super::*;

#[test]
fn detect_verify_commands_probes_repo_and_memory() {
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
    // MEMORY.md overrides on clash + adds new names
    std::fs::create_dir_all(root.join(".sqwai/memory")).unwrap();
    std::fs::write(
        root.join(".sqwai/memory/MEMORY.md"),
        "notes\nverify: test = make test-ci\nverify: lint = cargo clippy\n",
    )
    .unwrap();
    let found = Config::detect_verify_commands(root);
    let get = |n: &str| found.iter().find(|(k, _, _)| k == n).cloned();
    assert_eq!(
        get("test"),
        Some(("test".to_string(), "make test-ci".to_string(), "MEMORY.md"))
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
    cfg.apply_builtins();
    cfg.migrate_last_model();
    assert_eq!(cfg.last_model, "my-old");
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
    for p in ["gemini", "anthropic", "openai", "deepseek", "grok", "kimi"] {
        assert!(cfg.providers.contains_key(p), "missing provider {p}");
        assert!(cfg.is_builtin_provider(p), "not marked builtin {p}");
    }
    // Exact model ids float with the catalog (deepseek-chat became
    // v4-pro/v4-flash in the 2026-10-03 discovery sync), so assert
    // structurally: every builtin provider contributes builtin models.
    for p in ["gemini", "anthropic", "openai", "deepseek", "grok", "kimi"] {
        assert!(
            cfg.models
                .values()
                .any(|m| m.provider == p && cfg.is_builtin_model(&m.id)),
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

/// Audit H19: NaN survives `clamp` (both comparisons false) and
/// `NaN as u64` is 0 — a `budget_ratio = nan` override silently
/// zeroed every plan budget. Non-finite falls back to the default.
#[test]
fn non_finite_budget_ratio_falls_back_to_default() {
    let mut plan = PlanConfig::default();
    let default_budget = plan.budget_tokens(100_000);
    assert_eq!(default_budget, 10_000, "default ratio is 0.10");
    plan.budget_ratio = f64::NAN;
    assert_eq!(
        plan.budget_tokens(100_000),
        default_budget,
        "NaN must not zero the budget"
    );
    plan.budget_ratio = f64::INFINITY;
    assert_eq!(plan.budget_tokens(100_000), default_budget);
    plan.budget_ratio = 0.5;
    assert_eq!(plan.budget_tokens(100_000), 50_000);
    plan.budget_ratio = -1.0;
    assert_eq!(plan.budget_tokens(100_000), 0, "clamped, not defaulted");
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
    let key = "gemini-3.8-flash";
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
fn project_overrides_apply_allowlisted_keys_and_report_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let sqwai = dir.path().join(".sqwai");
    std::fs::create_dir_all(&sqwai).unwrap();
    std::fs::write(
        sqwai.join("config.toml"),
        r#"
[plan]
max_steps = 5
budget_ratio = 0.9
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
    // allowlisted keys land
    assert!(
        (cfg.plan.budget_ratio - 0.9).abs() < 1e-9,
        "budget_ratio is allowlisted: {}",
        cfg.plan.budget_ratio
    );
    // not allowlisted: reported, never applied
    assert!(
        notes.iter().any(|n| n.contains("plan.plan_first")),
        "{notes:?}"
    );
    assert!(notes.iter().any(|n| n.contains("[safety]")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("[models]")), "{notes:?}");
    // §11: a cloned repo must not widen the hosts our webfetch may reach
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
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.models["primary"].fallback.as_deref(), Some("secondary"));
    assert_eq!(cfg.models["secondary"].fallback, None);
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

/// §11: the opt-in opens exactly the loopback class, and only when the
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
