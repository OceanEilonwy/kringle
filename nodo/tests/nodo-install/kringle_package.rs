//! The Kringle recipe's own files, checked with the same code the device uses.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use nodo_install::{Permission, Permissions};
use nodo_manifest::{Manifest, Outbound};
use nodo_settings::{load_settings, Installed, InstalledPackage, Limits, Value};

fn files() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/kringle/files")
}

fn manifest() -> Manifest {
    let text = std::fs::read_to_string(files().join("manifest.toml")).unwrap();
    Manifest::parse(&text.replace("@VERSION@", "0.4.1")).unwrap()
}

#[test]
fn the_manifest_passes_feed_validation_and_names_its_files() {
    let m = manifest();
    assert_eq!(m.id, "kringle");
    assert_eq!(m.exec[0], "bin/kringle");
    assert!(files().join("kringle").exists());
    assert!(m.needs.is_empty());
    assert!(m.storage.encrypted, "names, hints, emails and the draw are user data");
    assert_eq!(m.network.outbound, Outbound::Internet, "email goes out through Tuta or SMTP");
    assert!(m.panels.is_empty(), "Kringle has no unauthenticated status endpoint");
    assert_eq!(m.web[0].upstream, "127.0.0.1:8787");
    let p = Permissions::of(&m, &[]);
    assert!(p.granted.contains(&Permission::OutboundInternet));
    assert!(!p.granted.contains(&Permission::UnencryptedStorage));
}

#[test]
fn settings_load_and_resolve_with_defaults_only() {
    let source = std::fs::read_to_string(files().join("settings.lua")).unwrap();
    let loaded = load_settings(&source, Limits::default()).unwrap();
    let schema = loaded.schema().clone();
    let mut installed = Installed::default();
    installed.packages.insert("kringle".into(), InstalledPackage { manifest: manifest(), schema, values: BTreeMap::new() });
    let resolved = installed.resolve("kringle").unwrap();
    assert_eq!(resolved.exec[1..], ["--addr", "127.0.0.1:8787", "--keep-days", "120"]);
    assert!(!resolved.values.contains_key("public_url"));
    assert!(resolved.dependencies.is_empty() && resolved.network.is_empty());
    let mut bad = resolved.values.clone();
    bad.insert("public_url".into(), Value::Url("https://kringle.example.org/swap".into()));
    bad.insert("keep_days".into(), Value::Int(0));
    assert!(!loaded.validate_values(&bad).is_empty());
    let mut good = resolved.values.clone();
    good.insert("public_url".into(), Value::Url("https://kringle.example.org".into()));
    assert!(loaded.validate_values(&good).is_empty());
}

fn launch(env: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let (bin, libexec, home) = (dir.path().join("bin"), dir.path().join("libexec"), dir.path().join("home"));
    for d in [&bin, &libexec, &home] {
        std::fs::create_dir(d).unwrap();
    }
    std::fs::copy(files().join("kringle"), bin.join("kringle")).unwrap();
    let stand_in = libexec.join("kringle");
    std::fs::write(&stand_in, "#!/bin/sh\nfor a do echo \"arg:$a\"; done\nenv | grep -E '^(KRINGLE_|XDG_CACHE_HOME=)' | sort\necho \"nodo:$(env | grep -c '^NODO_')\"\n").unwrap();
    std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new("sh");
    command.arg(bin.join("kringle")).args(["--addr", "127.0.0.1:8787"]);
    command.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &home);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).replace(home.to_str().unwrap(), "$HOME")
}

#[test]
fn the_launcher_passes_the_data_file_and_public_address() {
    assert_eq!(
        launch(&[("NODO_KEEP_DAYS", "120")]),
        "arg:--data\narg:$HOME/kringle.json\narg:--addr\narg:127.0.0.1:8787\nXDG_CACHE_HOME=$HOME/cache\nnodo:0\n"
    );
    // A KRINGLE_ variable the device did not set never reaches Kringle.
    assert_eq!(
        launch(&[("NODO_PUBLIC_URL", "https://kringle.example.org"), ("KRINGLE_MAIL", "smtp")]),
        "arg:--data\narg:$HOME/kringle.json\narg:--addr\narg:127.0.0.1:8787\n\
         arg:--public-url\narg:https://kringle.example.org\nXDG_CACHE_HOME=$HOME/cache\nnodo:0\n"
    );
}

#[test]
fn the_launcher_maps_the_email_settings() {
    assert_eq!(
        launch(&[
            ("NODO_MAIL", "tuta"),
            ("NODO_TUTA_USER", "kringle@tuta.com"),
            ("NODO_TUTA_PASSWORD", "s3cret pass"),
            ("NODO_SMTP_PORT", "587"),
            ("NODO_UNRELATED", "x"),
        ]),
        "arg:--data\narg:$HOME/kringle.json\narg:--addr\narg:127.0.0.1:8787\n\
         KRINGLE_MAIL=tuta\nKRINGLE_SMTP_PORT=587\nKRINGLE_TUTA_PASSWORD=s3cret pass\nKRINGLE_TUTA_USER=kringle@tuta.com\n\
         XDG_CACHE_HOME=$HOME/cache\nnodo:0\n"
    );
}

#[test]
fn email_settings_validate_and_default_to_off() {
    let source = std::fs::read_to_string(files().join("settings.lua")).unwrap();
    let loaded = load_settings(&source, Limits::default()).unwrap();
    let schema = loaded.schema().clone();
    for secret in ["tuta_password", "smtp_password"] {
        assert_eq!(schema.setting(secret).unwrap().ty.name(), "secret", "{secret}");
        assert!(schema.setting(secret).unwrap().public.is_none(), "{secret} is never exported");
    }
    let mut installed = Installed::default();
    installed.packages.insert("kringle".into(), InstalledPackage { manifest: manifest(), schema, values: BTreeMap::new() });
    let resolved = installed.resolve("kringle").unwrap();
    assert_eq!(resolved.values["mail"], Value::Enum("off".into()), "nothing is sent until the owner says so");
    let with = |name: &str, value: Value| {
        let mut values = resolved.values.clone();
        values.insert(name.into(), value);
        loaded.validate_values(&values)
    };
    assert!(!with("mail", Value::Enum("sendmail".into())).is_empty());
    assert!(!with("tuta_user", Value::String("kringle".into())).is_empty());
    assert!(!with("smtp_port", Value::Int(0)).is_empty());
    assert!(with("mail", Value::Enum("tuta".into())).is_empty());
    assert!(with("tuta_user", Value::String("kringle@tuta.com".into())).is_empty());
}
