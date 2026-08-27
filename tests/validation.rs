use std::collections::BTreeMap;
use std::path::Path;

use herdr_dev::manifest::{LoadError, Project};

const MANIFEST: &str = "/tmp/herdr-dev-tests/project/.herdr-dev.toml";

fn parse(text: &str) -> Result<Project, LoadError> {
    Project::parse(text, Path::new(MANIFEST))
}

fn rejection(text: &str) -> String {
    match parse(text) {
        Err(error) => error.to_string(),
        Ok(project) => panic!("accepted, with problems {:?}", project.problems),
    }
}

fn accepted(text: &str) -> Project {
    parse(text).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn a_syntax_error_names_the_file() {
    let complaint = rejection("[local.rails\ncmd = []\n");
    assert!(complaint.starts_with(MANIFEST), "{complaint}");
}

#[test]
fn duplicate_names_are_rejected_by_name() {
    let complaint = rejection(
        r#"
        [docker]
        names = ["db", "redis", "db"]
        "#,
    );
    assert!(complaint.contains("`db` twice"), "{complaint}");
}

#[test]
fn a_local_unit_without_cmd_breaks_only_itself() {
    let project = accepted(
        r#"
        [local.rails]
        cwd = "."

        [local.vite]
        cmd = ["bin/vite", "dev"]
        "#,
    );
    let rails = &project.local[0];
    assert!(rails.is_broken());
    assert_eq!(rails.problem.as_deref(), Some("[local.rails] has no `cmd`"));
    assert_eq!(project.problems, ["[local.rails] has no `cmd`"]);
    assert!(!project.local[1].is_broken());
    assert_eq!(project.local[1].cmd, ["bin/vite", "dev"]);
}

#[test]
fn a_shell_string_is_not_a_cmd() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = "bundle exec rails s"
        "#,
    );
    let problem = project.local[0].problem.clone().unwrap();
    assert!(
        problem.contains("`cmd` must be an array of strings"),
        "{problem}"
    );
    assert!(problem.contains("found string"), "{problem}");
    assert!(project.local[0].cmd.is_empty());
}

#[test]
fn a_cmd_element_that_is_not_a_string_is_caught_with_its_index() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = ["bin/rails", "server", "-p", 3000]
        "#,
    );
    let problem = project.local[0].problem.clone().unwrap();
    assert!(problem.contains("element 3 is integer"), "{problem}");
}

#[test]
fn an_empty_cmd_is_caught() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = []
        "#,
    );
    assert!(project.local[0].is_broken());
}

#[test]
fn an_unknown_top_level_key_is_rejected() {
    let complaint = rejection("ports = [3000]\n");
    assert!(
        complaint.contains("unknown top-level key `ports`"),
        "{complaint}"
    );
    assert!(complaint.contains("`local`"), "{complaint}");
}

#[test]
fn the_schema_that_was_dropped_stays_dropped() {
    for text in [
        "[[units]]\nname = \"rails\"\n",
        "[[groups]]\nname = \"deps\"\n",
        "version = 1\n",
        "compose_file = \"docker-compose.yml\"\n",
    ] {
        let complaint = rejection(text);
        assert!(complaint.contains("unknown top-level key"), "{complaint}");
    }
}

#[test]
fn an_unknown_docker_key_is_rejected() {
    let complaint = rejection(
        r#"
        [docker]
        names = ["db"]
        services = ["db"]
        "#,
    );
    assert!(complaint.contains("unknown key `services`"), "{complaint}");
}

#[test]
fn an_unknown_local_key_breaks_only_that_unit() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = ["bin/rails", "s"]
        optional = true

        [local.vite]
        cmd = ["bin/vite", "dev"]
        "#,
    );
    let problem = project.local[0].problem.clone().unwrap();
    assert!(problem.contains("unknown key `optional`"), "{problem}");
    assert!(!project.local[1].is_broken());
}

#[test]
fn a_unit_runs_on_a_terminal_only_where_it_asks_to() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = ["bin/rails", "s"]
        tty = true

        [local.vite]
        cmd = ["bin/vite", "dev"]

        [local.sidekiq]
        cmd = ["bundle", "exec", "sidekiq"]
        tty = false
        "#,
    );
    assert!(project.local[0].tty);
    assert!(
        !project.local[1].tty,
        "a unit that never asked for one must not get one"
    );
    assert!(!project.local[2].tty);
    assert!(project.local.iter().all(|unit| !unit.is_broken()));
}

#[test]
fn a_tty_that_is_not_a_boolean_breaks_only_its_own_unit() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = ["bin/rails", "s"]
        tty = "yes"

        [local.vite]
        cmd = ["bin/vite", "dev"]
        "#,
    );
    let problem = project.local[0].problem.clone().unwrap();
    assert!(problem.contains("`tty` must be a boolean"), "{problem}");
    assert!(problem.contains("[local.rails]"), "{problem}");
    assert!(
        !project.local[0].tty,
        "a unit the reader refused must not run on a terminal"
    );
    assert!(!project.local[1].is_broken());
}

#[test]
fn a_name_may_be_both_one_shot_and_hidden() {
    let project = accepted(
        r#"
        [docker]
        names = ["db"]
        one_shot = ["minio_init"]
        hidden = ["minio", "minio_init"]
        "#,
    );
    assert!(project.problems.is_empty(), "{:?}", project.problems);
    assert_eq!(
        project
            .docker
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["db"]
    );
}

#[test]
fn hidden_services_never_reach_the_model() {
    let project = accepted(
        r#"
        [docker]
        names = ["db", "redis"]
        hidden = ["web", "frontend"]

        [docker.notes]
        db = "the one that matters"
        "#,
    );
    let rendered: Vec<&str> = project.docker.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(rendered, ["db", "redis"]);
    assert!(!format!("{project:?}").contains("web"), "{project:?}");
}

#[test]
fn hidden_wins_over_names_and_says_so() {
    let project = accepted(
        r#"
        [docker]
        names = ["db", "web"]
        hidden = ["web"]
        "#,
    );
    assert_eq!(
        project
            .docker
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["db"]
    );
    assert_eq!(project.problems.len(), 1);
    assert!(
        project.problems[0].contains("`web` is in both"),
        "{:?}",
        project.problems
    );
}

#[test]
fn a_one_shot_that_is_declared_nowhere_is_a_complaint_not_a_rejection() {
    let project = accepted(
        r#"
        [docker]
        names = ["db"]
        one_shot = ["migrate"]
        "#,
    );
    assert_eq!(
        project
            .docker
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["db"]
    );
    assert!(
        project.problems[0].contains("`migrate`"),
        "{:?}",
        project.problems
    );
}

#[test]
fn a_note_for_a_service_that_is_never_rendered_is_a_complaint() {
    let project = accepted(
        r#"
        [docker]
        names = ["db"]
        hidden = ["web"]

        [docker.notes]
        web = "unreachable"
        typo = "also unreachable"
        "#,
    );
    assert_eq!(project.problems.len(), 2);
    assert!(project.problems.iter().any(|p| p.contains("`typo`")));
    assert!(project.problems.iter().any(|p| p.contains("hidden")));
}

#[test]
fn env_layers_process_then_top_level_then_unit() {
    let project = accepted(
        r#"
        [env]
        CURRENT_WORKTREE = "wt2"
        VITE_RUBY_PORT = "3036"

        [local.vite]
        cmd = ["bin/vite", "dev"]
        env = { VITE_RUBY_PORT = "3136" }
        "#,
    );
    let vite = &project.local[0];
    assert_eq!(vite.env["CURRENT_WORKTREE"], "wt2");
    assert_eq!(vite.env["VITE_RUBY_PORT"], "3136");
    assert_eq!(project.env["VITE_RUBY_PORT"], "3036");

    let layered = vite.env_over([("PATH", "/usr/bin"), ("CURRENT_WORKTREE", "from-the-shell")]);
    assert_eq!(layered["PATH"], "/usr/bin");
    assert_eq!(layered["CURRENT_WORKTREE"], "wt2");
    assert_eq!(layered["VITE_RUBY_PORT"], "3136");
}

#[test]
fn a_unit_without_env_still_carries_the_top_level_layer() {
    let project = accepted(
        r#"
        [env]
        CURRENT_WORKTREE = "wt2"

        [local.rails]
        cmd = ["bin/rails", "s"]
        "#,
    );
    assert_eq!(
        project.local[0].env,
        BTreeMap::from([("CURRENT_WORKTREE".to_string(), "wt2".to_string())])
    );
}

#[test]
fn an_env_value_must_be_a_string() {
    let complaint = rejection("[env]\nPORT = 3000\n");
    assert!(
        complaint.contains("`env.PORT` must be a string"),
        "{complaint}"
    );

    let project = accepted("[local.rails]\ncmd = [\"x\"]\nenv = { PORT = 3000 }\n");
    let problem = project.local[0].problem.clone().unwrap();
    assert!(problem.contains("`local.rails.env.PORT`"), "{problem}");
}

#[test]
fn document_order_is_preserved_rather_than_sorted() {
    let project = accepted(
        r#"
        [local.vite]
        cmd = ["bin/vite", "dev"]

        [local.rails]
        cmd = ["bin/rails", "s"]

        [docker]
        names = ["redis", "db"]

        [includes.zulu]
        path = "~/z"

        [includes.alpha]
        path = "~/a"
        "#,
    );
    assert_eq!(
        project
            .local
            .iter()
            .map(|u| u.name.as_str())
            .collect::<Vec<_>>(),
        ["vite", "rails"]
    );
    assert_eq!(
        project
            .docker
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["redis", "db"]
    );
    assert_eq!(
        project
            .includes
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["zulu", "alpha"]
    );
}

#[test]
fn a_relative_cwd_hangs_off_the_manifests_directory_and_tilde_off_home() {
    let project = accepted(
        r#"
        [local.rails]
        cmd = ["bin/rails", "s"]
        cwd = "backend"

        [local.vite]
        cmd = ["bin/vite", "dev"]
        cwd = "~/projects/acme/storefront"
        "#,
    );
    assert_eq!(
        project.local[0].cwd,
        Path::new("/tmp/herdr-dev-tests/project/backend")
    );
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        project.local[1].cwd,
        Path::new(&home).join("projects/acme/storefront")
    );
}

#[test]
fn an_include_without_a_path_complains_and_leaves_the_others_alone() {
    let project = accepted(
        r#"
        [includes.accounts]
        paht = "~/projects/acme/accounts"

        [includes.billing]
        path = "~/projects/acme/billing"
        "#,
    );
    assert_eq!(
        project
            .includes
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["billing"]
    );
    assert!(
        project.problems[0].contains("[includes.accounts]"),
        "{:?}",
        project.problems
    );
}

#[test]
fn a_section_of_the_wrong_shape_is_rejected() {
    for (text, expected) in [
        ("local = \"rails\"\n", "`local` must be a table"),
        ("docker = []\n", "`docker` must be a table"),
        ("includes = 3\n", "`includes` must be a table"),
        ("env = \"none\"\n", "`env` must be a table"),
        (
            "[docker]\nnames = \"db\"\n",
            "`docker.names` must be an array",
        ),
        (
            "[docker]\nhidden = [\"web\", 3]\n",
            "`docker.hidden` must be an array of strings, but element 1 is integer",
        ),
    ] {
        let complaint = rejection(text);
        assert!(
            complaint.contains(expected),
            "{complaint} does not mention {expected}"
        );
    }
}

#[test]
fn an_empty_manifest_is_legal_and_empty() {
    let project = accepted("");
    assert!(project.is_empty());
    assert!(project.problems.is_empty());
    assert_eq!(project.root, Path::new("/tmp/herdr-dev-tests/project"));
    assert_eq!(project.name, "project");
}

#[test]
fn a_missing_manifest_names_the_path() {
    let error = Project::load("/tmp/herdr-dev-tests/nowhere/.herdr-dev.toml").unwrap_err();
    assert!(matches!(error, LoadError::Read { .. }));
    assert!(error.to_string().contains("/tmp/herdr-dev-tests/nowhere"));
}
