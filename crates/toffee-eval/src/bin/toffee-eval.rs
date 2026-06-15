//! `toffee-eval` — measure, tune, and debug toffee's memory quality.
//!
//!   toffee-eval [run]        score the corpus (default; gates in CI)
//!   toffee-eval tune         fit the read-path weights to the retrieval corpus
//!   toffee-eval debug ...     introspect one extraction input or retrieval case
//!
//! `run` exits non-zero on a missed gate. `tune --write` persists the fitted
//! weights to config.toml, which a running daemon hot-reloads.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use toffee_core::{paths, Config, ScoringConfig};
use toffee_eval::corpus::{self, ExtractionCase, RetrievalCase};
use toffee_eval::{check_thresholds, debug, extraction, retrieval, tune, Thresholds};

#[derive(Parser)]
#[command(about = "Measure, tune, and debug toffee's memory quality")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Score the corpus and print a scorecard (default).
    Run(RunArgs),
    /// Fit the read-path scoring weights to the retrieval corpus.
    Tune(TuneArgs),
    /// Introspect a single extraction input or retrieval case.
    Debug(DebugArgs),
}

#[derive(Parser)]
struct RunArgs {
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/extraction.jsonl"))]
    extraction: PathBuf,
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/retrieval.jsonl"))]
    retrieval: PathBuf,
    /// Score retrieval under the weights in this config file instead of the
    /// built-in defaults.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// List every failing / spurious case.
    #[arg(long)]
    verbose: bool,
    /// Score and print, but never exit non-zero on a missed gate.
    #[arg(long)]
    no_gate: bool,
}

#[derive(Parser)]
struct TuneArgs {
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/retrieval.jsonl"))]
    retrieval: PathBuf,
    /// Coordinate-descent passes over the parameter set.
    #[arg(long, default_value_t = 4)]
    passes: usize,
    /// Persist the fitted weights. Writes to --out (default:
    /// $XDG_CONFIG_HOME/toffee/config.toml), which a running daemon picks up.
    #[arg(long)]
    write: bool,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Parser)]
struct DebugArgs {
    #[command(subcommand)]
    what: DebugWhat,
}

#[derive(Subcommand)]
enum DebugWhat {
    /// Show what the extractor produces for a line of text.
    Extract { text: String },
    /// Replay a retrieval case and show the per-memory score breakdown.
    Retrieve {
        case: String,
        #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/retrieval.jsonl"))]
        retrieval: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => cmd_run(RunArgs::parse_from(["run"])),
        Some(Command::Run(a)) => cmd_run(a),
        Some(Command::Tune(a)) => cmd_tune(a),
        Some(Command::Debug(a)) => cmd_debug(a),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Load the scoring weights from a config file, or fall back to defaults.
fn scoring_from(config: &Option<PathBuf>) -> Result<ScoringConfig> {
    match config {
        None => Ok(ScoringConfig::default()),
        Some(path) => {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let cfg: Config = toml::from_str(&body)
                .with_context(|| format!("parsing config {}", path.display()))?;
            Ok(cfg.scoring)
        }
    }
}

fn cmd_run(args: RunArgs) -> Result<bool> {
    let extraction_cases: Vec<ExtractionCase> = corpus::load_jsonl(&args.extraction)?;
    let retrieval_cases: Vec<RetrievalCase> = corpus::load_jsonl(&args.retrieval)?;
    let scoring = scoring_from(&args.config)?;

    let ext = extraction::run(&extraction_cases);
    let ret = retrieval::run_with(&retrieval_cases, &scoring)?;

    let thresholds = Thresholds::default();
    let failures = check_thresholds(&ext.score, &ret.score, &thresholds);

    if args.json {
        print_json(&ext, &ret, &failures);
    } else {
        print_table(&ext, &ret, &thresholds, args.verbose);
        if !failures.is_empty() {
            println!("\nGATE FAILED:");
            for f in &failures {
                println!("  ✗ {f}");
            }
        }
    }
    Ok(failures.is_empty() || args.no_gate)
}

fn cmd_tune(args: TuneArgs) -> Result<bool> {
    let cases: Vec<RetrievalCase> = corpus::load_jsonl(&args.retrieval)?;
    let result = tune::tune(&cases, ScoringConfig::default(), args.passes)?;

    let fmt = |c: &ScoringConfig| {
        format!(
            "vec={:.3} ent={:.3} rec={:.3} conf={:.3} tau={:.0}d",
            c.vector_weight,
            c.entity_weight,
            c.recency_weight,
            c.confidence_weight,
            c.recency_half_life_days
        )
    };
    println!("== tune (retrieval corpus, {} cases) ==", cases.len());
    println!(
        "  objective          0.5*MRR + 0.5*nDCG@{}",
        result.start_score.k
    );
    println!("  start   {}", fmt(&result.start));
    println!(
        "          fitness {:.4}  (MRR {:.4}  nDCG {:.4})",
        result.start_fitness, result.start_score.mrr, result.start_score.ndcg_at_k
    );
    println!("  best    {}", fmt(&result.best));
    println!(
        "          fitness {:.4}  (MRR {:.4}  nDCG {:.4})",
        result.best_fitness, result.best_score.mrr, result.best_score.ndcg_at_k
    );

    // Guard against the classic small-corpus failure: the optimizer happily
    // zeroes a signal that just happens to be unhelpful on a handful of cases
    // (e.g. the hash embedder's weak vector score), which would hurt the real
    // BGE read path. Surface that rather than letting it silently --write.
    const MIN_TRUSTWORTHY_CASES: usize = 30;
    if result.improved() {
        println!(
            "  → +{:.4} fitness over the baseline weights",
            result.best_fitness - result.start_fitness
        );
        let zeroed = result.best.vector_weight == 0.0
            || result.best.entity_weight == 0.0
            || result.best.recency_weight == 0.0;
        if cases.len() < MIN_TRUSTWORTHY_CASES || zeroed {
            println!(
                "  ⚠ small corpus ({} cases) — this gain is likely overfit.",
                cases.len()
            );
            if zeroed {
                println!("    A weight was driven to 0; verify against real (BGE) retrieval");
                println!("    and a larger corpus before --write.");
            }
        }
    } else {
        println!("  → no improvement: the baseline already maxes this corpus.");
        println!("    Add adversarial cases (near-duplicate distractors, paraphrase)");
        println!("    so the objective can discriminate before trusting a tune.");
    }

    if args.write {
        let out = args.out.unwrap_or_else(paths::config_file);
        write_config(&out, result.best)?;
        println!(
            "\n  wrote {} (a running daemon will hot-reload it)",
            out.display()
        );
    }
    Ok(true)
}

fn write_config(path: &Path, scoring: ScoringConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(&Config { scoring }).context("serializing config")?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn cmd_debug(args: DebugArgs) -> Result<bool> {
    match args.what {
        DebugWhat::Extract { text } => {
            print!("{}", debug::extract_report(&text));
        }
        DebugWhat::Retrieve {
            case,
            retrieval,
            config,
        } => {
            let cases: Vec<RetrievalCase> = corpus::load_jsonl(&retrieval)?;
            let found = cases
                .iter()
                .find(|c| c.name == case)
                .with_context(|| format!("no retrieval case named '{case}'"))?;
            let scoring = scoring_from(&config)?;
            print!("{}", debug::retrieve_report(found, &scoring)?);
        }
    }
    Ok(true)
}

fn print_table(
    ext: &extraction::ExtractionReport,
    ret: &retrieval::RetrievalReport,
    t: &Thresholds,
    verbose: bool,
) {
    let e = &ext.score;
    println!("== Extraction (event → memory) ==");
    println!(
        "  cases              {} ({} negative)",
        e.cases, e.negative_cases
    );
    println!(
        "  tp / fp / fn       {} / {} / {}",
        e.true_positives, e.false_positives, e.false_negatives
    );
    println!(
        "  precision          {:.3}   (min {:.2})",
        e.precision(),
        t.extraction_precision
    );
    println!(
        "  recall             {:.3}   (min {:.2})",
        e.recall(),
        t.extraction_recall
    );
    println!("  F0.5               {:.3}", e.f_beta(0.5));
    println!(
        "  field accuracy     kind {:.3}  subj {:.3}  pred {:.3}  obj {:.3}",
        e.kind.value(),
        e.subject.value(),
        e.predicate.value(),
        e.object.value()
    );

    let r = &ret.score;
    println!("\n== Retrieval (query → context) ==");
    println!("  cases              {}", r.cases);
    println!(
        "  package recall     {:.3}   (min {:.2})",
        r.package_recall, t.retrieval_package_recall
    );
    println!("  bucket accuracy    {:.3}", r.bucket.value());
    println!(
        "  MRR                {:.3}   (min {:.2})",
        r.mrr, t.retrieval_mrr
    );
    println!("  Recall@{:<2}          {:.3}", r.k, r.recall_at_k);
    println!("  nDCG@{:<2}            {:.3}", r.k, r.ndcg_at_k);

    if verbose {
        if !ext.misses.is_empty() {
            println!("\n  extraction misses:");
            for m in &ext.misses {
                println!("    [{}] {}", m.name, m.note);
            }
        }
        if !ret.misses.is_empty() {
            println!("\n  retrieval misses:");
            for m in &ret.misses {
                println!("    [{}] {}", m.name, m.note);
            }
        }
    }
}

fn print_json(
    ext: &extraction::ExtractionReport,
    ret: &retrieval::RetrievalReport,
    failures: &[String],
) {
    let e = &ext.score;
    let r = &ret.score;
    let out = serde_json::json!({
        "extraction": {
            "cases": e.cases,
            "true_positives": e.true_positives,
            "false_positives": e.false_positives,
            "false_negatives": e.false_negatives,
            "precision": e.precision(),
            "recall": e.recall(),
            "f0_5": e.f_beta(0.5),
            "field_accuracy": {
                "kind": e.kind.value(),
                "subject": e.subject.value(),
                "predicate": e.predicate.value(),
                "object": e.object.value(),
            },
        },
        "retrieval": {
            "cases": r.cases,
            "package_recall": r.package_recall,
            "bucket_accuracy": r.bucket.value(),
            "mrr": r.mrr,
            "recall_at_k": r.recall_at_k,
            "ndcg_at_k": r.ndcg_at_k,
            "k": r.k,
        },
        "gate_failures": failures,
        "passed": failures.is_empty(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
