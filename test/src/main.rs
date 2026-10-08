//! `/test short|medium|long` — stormdrive's test container (#11). See lib.rs.

#[tokio::main]
async fn main() {
    let suite = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("STORM_SUITE").ok())
        .unwrap_or_else(|| "short".into());
    if !["short", "medium", "long"].contains(&suite.as_str()) {
        eprintln!("usage: test short|medium|long");
        std::process::exit(2);
    }
    // stdout is the JSON report; retries (#71) are logged on stderr.
    retry::log_to_stderr(true);
    let env = stormdrive_test::env::Env::read(&suite);
    let mut r = stormdrive_test::report::Report::new(&env.results);
    let code = stormdrive_test::run(&suite, &env, &mut r).await;
    r.summary();
    std::process::exit(code);
}
