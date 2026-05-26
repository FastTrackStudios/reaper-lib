use std::process::ExitCode;

fn main() -> ExitCode {
    fts_repo::dispatch(
        &fts_repo::XtaskConfig {
            nextest_profile: "ci".into(),
            run_doctests: false,
            run_tracey: false,
            ..Default::default()
        },
        |_c, _r| None,
    )
}
