use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-dev"))
        .args(args)
        .env_remove("HERDR_DEV_LOG")
        .output()
        .expect("herdr-dev runs")
}

#[test]
fn tail_mode_is_selected_by_argv_and_says_what_it_is_missing() {
    let output = run(&["tail"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("HERDR_DEV_LOG"), "tail said {stderr}");
}

#[test]
fn the_tui_refuses_to_start_without_a_terminal() {
    let output = run(&[]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs a terminal"), "{stderr}");
}

#[test]
fn unknown_argv_exits_non_zero_with_a_usage_line() {
    for args in [&["serve"][..], &["--help"][..], &["tail", "extra"][..]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("usage: herdr-dev"),
            "{args:?} said {stderr}"
        );
    }
}
