//! Thalvi measurement tool: times and grades replies for one or more setups.

use std::path::{Path, PathBuf};

use serde_json::json;
use thalvi_server::judge::{Grade, Judge, judge_all};
use thalvi_server::measure::{Case, ConfiguredTts, Setup, load_setup, parse_cases, run_all};
use thalvi_server::model::OpenAiChatModel;
use thalvi_server::report::{NOTE, render_table, summarize};

const DEFAULT_CASES: &str = include_str!("../../measure/cases.json");

const USAGE: &str = "usage: measure [--setup <env file>]... [--runs <n>] [--cases <file>] [--only <case id>]... [--no-judge]";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    setups: Vec<PathBuf>,
    runs: usize,
    cases: Option<PathBuf>,
    only: Vec<String>,
    no_judge: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        setups: Vec::new(),
        runs: 3,
        cases: None,
        only: Vec::new(),
        no_judge: false,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--setup" => parsed.setups.push(PathBuf::from(value()?)),
            "--runs" => {
                let raw = value()?;
                parsed.runs = raw
                    .parse()
                    .ok()
                    .filter(|&runs| runs > 0)
                    .ok_or_else(|| format!("--runs must be a positive number: {raw:?}"))?;
            }
            "--cases" => parsed.cases = Some(PathBuf::from(value()?)),
            "--only" => parsed.only.push(value()?),
            "--no-judge" => parsed.no_judge = true,
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(parsed)
}

fn setup_name(path: &Path) -> String {
    path.file_stem().map_or_else(
        || "setup".to_string(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn load_cases(args: &Args) -> Result<Vec<Case>, String> {
    let json = match &args.cases {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?,
        None => DEFAULT_CASES.to_string(),
    };
    let mut cases = parse_cases(&json)?;
    if !args.only.is_empty() {
        if let Some(id) = args
            .only
            .iter()
            .find(|id| !cases.iter().any(|case| &case.id == *id))
        {
            return Err(format!("no case with id {id:?}"));
        }
        cases.retain(|case| args.only.contains(&case.id));
    }
    Ok(cases)
}

fn load_setups(args: &Args) -> Result<Vec<Setup<OpenAiChatModel, ConfiguredTts>>, String> {
    let mut setups = Vec::new();
    if args.setups.is_empty() {
        setups.push(load_setup("default", "", env)?);
    }
    for path in &args.setups {
        let name = setup_name(path);
        if setups.iter().any(|setup| setup.name == name) {
            return Err(format!("two setups are named {name:?}; rename a file"));
        }
        let overrides = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        setups
            .push(load_setup(&name, &overrides, env).map_err(|error| format!("{name}: {error}"))?);
    }
    Ok(setups)
}

async fn run() -> Result<(), String> {
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        return Err(format!("could not load .env: {error}"));
    }
    let args = parse_args(std::env::args().skip(1))?;
    let cases = load_cases(&args)?;
    let setups = load_setups(&args)?;
    let judge = if args.no_judge {
        None
    } else {
        Judge::from_lookup(env)?
    };

    eprintln!(
        "Measuring {} setup(s) on {} case(s), {} run(s) each.",
        setups.len(),
        cases.len(),
        args.runs
    );
    let mut samples = run_all(&setups, &cases, args.runs, |sample| {
        let m = &sample.measured;
        let seconds =
            |value: Option<f64>| value.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        eprintln!(
            "{} {} run {}: first audio {} s, end to end {} s{}",
            sample.setup,
            sample.case,
            sample.run + 1,
            seconds(m.first_audio_s),
            seconds(m.end_to_end_s),
            m.error
                .as_ref()
                .map_or_else(String::new, |e| format!(", ERROR: {e}")),
        );
    })
    .await;
    if let Some(judge) = &judge {
        eprintln!("Judging with {}.", judge.model());
        judge_all(judge, &cases, &mut samples, |sample| {
            let outcome = match &sample.grade {
                Some(Grade::Scored { score, note }) => format!("{score}: {note}"),
                Some(Grade::Unparsed { .. }) => "unparsed".to_string(),
                Some(Grade::Failed { error }) => format!("failed: {error}"),
                None => String::new(),
            };
            eprintln!(
                "{} {} run {}: {outcome}",
                sample.setup,
                sample.case,
                sample.run + 1
            );
        })
        .await;
    }

    let summaries: Vec<_> = setups
        .iter()
        .map(|setup| summarize(&setup.name, &setup.label, &samples))
        .collect();
    println!("\n{}", render_table(&summaries));
    println!("{NOTE}");
    if judge.is_none() {
        println!(
            "Quality was not judged: set JUDGE_BASE_URL and JUDGE_MODEL (and JUDGE_API_KEY) to grade replies."
        );
    }

    let data_dir = env("THALVI_DATA_DIR")
        .filter(|dir| !dir.trim().is_empty())
        .unwrap_or_else(|| "data".to_string());
    let dir = Path::new(&data_dir).join("measure");
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let path = dir.join(format!("run-{started}.json"));
    let results = json!({
        "started_unix": started,
        "runs": args.runs,
        "judge_model": judge.as_ref().map(Judge::model),
        "setups": setups.iter().map(|s| json!({"name": s.name, "label": s.label})).collect::<Vec<_>>(),
        "samples": samples,
    });
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, serde_json::to_vec_pretty(&results)?))
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    println!("Raw results: {}", path.display());
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, parse_args};
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn flags_set_options_and_defaults_apply() {
        assert_eq!(
            parse(&[]),
            Ok(Args {
                setups: vec![],
                runs: 3,
                cases: None,
                only: vec![],
                no_judge: false,
            })
        );
        assert_eq!(
            parse(&[
                "--setup",
                "a.env",
                "--setup",
                "b.env",
                "--runs",
                "5",
                "--cases",
                "c.json",
                "--only",
                "hello",
                "--only",
                "times",
                "--no-judge",
            ]),
            Ok(Args {
                setups: vec![PathBuf::from("a.env"), PathBuf::from("b.env")],
                runs: 5,
                cases: Some(PathBuf::from("c.json")),
                only: vec!["hello".to_string(), "times".to_string()],
                no_judge: true,
            })
        );
    }

    #[test]
    fn bad_flags_are_named() {
        for (args, mentions) in [
            (vec!["--runs", "0"], "--runs"),
            (vec!["--runs", "many"], "--runs"),
            (vec!["--setup"], "--setup"),
            (vec!["--fast"], "--fast"),
        ] {
            let err = parse(&args).unwrap_err();
            assert!(err.contains(mentions), "{args:?}: {err}");
        }
    }
}
