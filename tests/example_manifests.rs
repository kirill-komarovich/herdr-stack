use std::path::{Path, PathBuf};

use herdr_dev::manifest::Project;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/manifests");

fn load(project: &str) -> Project {
    let path = PathBuf::from(DIR).join(format!("{project}.herdr-dev.toml"));
    Project::load(&path).unwrap_or_else(|error| panic!("{error}"))
}

fn service_names(project: &Project) -> Vec<&str> {
    project.docker.iter().map(|s| s.name.as_str()).collect()
}

fn unit_names(project: &Project) -> Vec<&str> {
    project.local.iter().map(|u| u.name.as_str()).collect()
}

#[test]
fn every_example_manifest_parses_without_a_single_problem() {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(DIR).expect("example-manifests directory") {
        let path = entry.unwrap().path();
        if path.extension() != Some(std::ffi::OsStr::new("toml")) {
            continue;
        }
        let project = Project::load(&path).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(project.problems, Vec::<String>::new(), "{}", path.display());
        assert!(!project.is_empty(), "{}", path.display());
        assert!(
            project.local.iter().all(|unit| !unit.is_broken()),
            "{}",
            path.display()
        );
        found.push(path.file_name().unwrap().to_string_lossy().into_owned());
    }
    found.sort();
    assert_eq!(
        found,
        [
            "billing.herdr-dev.toml",
            "storefront.herdr-dev.toml",
            "backoffice.herdr-dev.toml",
            "photos.herdr-dev.toml",
            "accounts.herdr-dev.toml",
        ]
    );
}

#[test]
fn photos_is_one_local_unit_and_no_rendered_service() {
    let photos = load("photos");
    assert_eq!(unit_names(&photos), ["phoenix"]);
    assert_eq!(photos.local[0].cmd, ["mix", "phx.server"]);
    assert_eq!(service_names(&photos), Vec::<&str>::new());
    assert!(photos.env.is_empty());
    assert!(photos.includes.is_empty());
}

#[test]
fn storefront_renders_one_service_and_three_included_repos() {
    let storefront = load("storefront");
    assert_eq!(unit_names(&storefront), ["rails", "vite"]);
    assert_eq!(service_names(&storefront), ["db"]);
    assert_eq!(
        storefront
            .includes
            .iter()
            .map(|include| include.name.as_str())
            .collect::<Vec<_>>(),
        ["accounts", "billing", "backoffice"]
    );
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        storefront.includes[0].path,
        Path::new(&home).join("projects/acme/accounts")
    );
}

#[test]
fn storefront_marks_a_hidden_service_one_shot_without_rendering_it() {
    let storefront = load("storefront");
    assert!(!storefront.docker.iter().any(|s| s.name == "minio_init"));
    assert!(storefront.problems.is_empty());
}

#[test]
fn billing_keeps_names_order_and_carries_one_shot_and_notes() {
    let billing = load("billing");
    assert_eq!(unit_names(&billing), ["rails", "sidekiq", "vite"]);
    assert_eq!(
        service_names(&billing),
        [
            "db",
            "redis",
            "memcached",
            "mongo",
            "dragonfly",
            "redis-sentinel-1",
            "redis-sentinel-2",
            "storefront",
            "migrate",
        ]
    );
    let migrate = billing.docker.last().unwrap();
    assert!(migrate.one_shot);
    assert_eq!(
        migrate.note.as_deref(),
        Some("declares no restart: key, so only a human can mark it one-shot")
    );
    assert!(billing.docker[..8].iter().all(|s| !s.one_shot));
    assert!(billing.docker[..8].iter().all(|s| s.note.is_none()));
}

#[test]
fn accounts_notes_a_rendered_service() {
    let accounts = load("accounts");
    assert_eq!(
        service_names(&accounts),
        ["db", "redis", "memcached", "mongo", "storefront"]
    );
    let storefront = accounts.docker.last().unwrap();
    assert_eq!(storefront.name, "storefront");
    assert_eq!(
        storefront.note.as_deref(),
        Some("run `docker compose run --rm storefront rails db:create db:migrate` once, by hand")
    );
}

#[test]
fn backoffice_declares_no_one_shot() {
    let backoffice = load("backoffice");
    assert_eq!(unit_names(&backoffice), ["rails", "sidekiq", "vite"]);
    assert_eq!(
        service_names(&backoffice),
        ["db", "redis", "memcached", "storefront"]
    );
    assert!(backoffice.docker.iter().all(|service| !service.one_shot));
}

#[test]
fn a_local_unit_defaults_to_the_manifests_directory() {
    let photos = load("photos");
    assert_eq!(photos.local[0].cwd, photos.root);
    assert_eq!(photos.manifest.parent().unwrap(), photos.root);
}
