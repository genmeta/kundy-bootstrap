fn main() {
    if let Err(error) = genmeta_xtask_release::runner::run_current_dir() {
        eprintln!("{}", snafu::Report::from_error(&error));
        std::process::exit(1);
    }
}
