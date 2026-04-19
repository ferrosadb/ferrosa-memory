use clap::Parser;

#[derive(Parser)]
#[command(name = "ferrosa-memory-eval", about = "MCP evaluation framework")]
struct Cli {
    /// Scenario files or directories to run
    #[arg(default_value = "scenarios/")]
    scenarios: Vec<String>,

    /// Run only Level 1, 2, 3, or all
    #[arg(long, default_value = "all")]
    level: String,

    /// Run only scenarios matching this tag
    #[arg(long)]
    tag: Option<String>,

    /// Skip LLM-as-Judge grading
    #[arg(long)]
    no_llm_judge: bool,

    /// Output raw JSON instead of formatted text
    #[arg(long)]
    json: bool,

    /// Stop on first failing scenario
    #[arg(long)]
    fail_fast: bool,

    /// Run N scenarios in parallel
    #[arg(long, default_value = "1")]
    parallel: usize,

    /// Show individual tool call traces
    #[arg(long)]
    verbose: bool,

    /// Run with LLM judge enabled
    #[arg(long)]
    with_judge: bool,

    /// Run red-team scenarios only
    #[arg(long)]
    red_team: bool,
}

fn main() {
    let _cli = Cli::parse();
    eprintln!("ferrosa-memory-eval: not yet implemented");
    std::process::exit(1);
}
