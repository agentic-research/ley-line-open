#![cfg(not(feature = "cdc"))]

use clap::Parser;

#[derive(Parser)]
#[command(name = "leyline")]
struct TestCli {
    #[command(subcommand)]
    command: leyline_cli_lib::Commands,
}

#[tokio::test]
async fn cdc_command_parses_and_returns_an_actionable_feature_error() {
    let cli = TestCli::try_parse_from(["leyline", "cdc", "enable", "--db", "graph.db"])
        .expect("the command shape must remain discoverable without the feature");

    let error = leyline_cli_lib::run(cli.command)
        .await
        .expect_err("a feature-disabled build must fail explicitly");
    assert_eq!(
        error.to_string(),
        "cdc enable requires the 'cdc' feature (compile with --features cdc)"
    );

    let cli = TestCli::try_parse_from(["leyline", "cdc", "gc", "--db", "graph.db", "--dry-run"])
        .expect("the GC command shape must remain discoverable without the feature");
    let error = leyline_cli_lib::run(cli.command)
        .await
        .expect_err("a feature-disabled GC build must fail explicitly");
    assert_eq!(
        error.to_string(),
        "cdc gc requires the 'cdc' feature (compile with --features cdc)"
    );
}
