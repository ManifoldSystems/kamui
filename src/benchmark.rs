use crate::config::{Config, Profile};
use crate::provider::{ChatRequest, Message, Provider};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    prompt: String,
    #[serde(default)]
    follow_up_prompt: Option<String>,
    #[serde(default)]
    expect_contains: Vec<String>,
}

impl Case {
    fn prompt_for_run(&self, run: usize) -> &str {
        if run > 1 {
            self.follow_up_prompt.as_deref().unwrap_or(&self.prompt)
        } else {
            &self.prompt
        }
    }
}

#[derive(Default)]
struct Totals {
    passed: usize,
    runs: usize,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cached_tokens: u64,
    cache_hits: Vec<f64>,
    warmups: usize,
    latency: Duration,
}

pub async fn run<F>(
    config: &Config,
    suite_path: &Path,
    profile_name: Option<&str>,
    runs: usize,
    build_provider: F,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    let profile = match profile_name {
        Some(name) => config
            .find(name)
            .with_context(|| format!("unknown profile '{name}'"))?,
        None => config.default(),
    };
    let suite = load_suite(suite_path)?;
    let provider = build_provider(profile);
    let mut totals = Totals::default();

    println!(
        "Benchmark: {} case(s) x {runs} run(s) on {} ({})",
        suite.cases.len(),
        profile.model,
        profile.name
    );
    println!();

    for case in &suite.cases {
        let session_id = profile.send_session_id.then(|| Uuid::new_v4().to_string());
        let mut messages = Vec::new();
        for run in 1..=runs {
            messages.push(Message::user(case.prompt_for_run(run)));
            let started = Instant::now();
            let response = provider
                .chat(ChatRequest {
                    model: profile.model.clone(),
                    messages: messages.clone(),
                    tools: Vec::new(),
                    session_id: session_id.clone(),
                })
                .await
                .with_context(|| format!("benchmark case '{}' failed", case.name))?;
            let elapsed = started.elapsed();
            let missing = missing_expectations(&response.content, &case.expect_contains);
            let passed = missing.is_empty();

            totals.runs += 1;
            totals.passed += usize::from(passed);
            totals.latency += elapsed;
            totals.input_tokens += response.usage.prompt_tokens;
            totals.output_tokens += response.usage.completion_tokens;
            totals.total_tokens += response.usage.total_tokens;
            totals.cached_tokens += response.usage.cached_tokens;
            if profile.send_session_id {
                if response.usage.prompt_tokens > 0 && response.usage.cached_tokens == 0 {
                    totals.warmups += 1;
                }
                if run > 1 && response.usage.prompt_tokens > 0 {
                    totals.cache_hits.push(
                        (response.usage.cached_tokens as f64 / response.usage.prompt_tokens as f64
                            * 100.0)
                            .min(100.0),
                    );
                }
            }

            let mark = if passed { "PASS" } else { "FAIL" };
            println!(
                "  {mark:<4} {:<28} run {:>2}  {:>7.2}s  {:>7} tokens",
                case.name,
                run,
                elapsed.as_secs_f64(),
                response.usage.total_tokens
            );
            if !missing.is_empty() {
                println!("       missing: {}", missing.join(", "));
            }
            messages.push(Message::assistant(response.content));
        }
    }

    let average = totals.latency.as_secs_f64() / totals.runs as f64;
    println!();
    println!(
        "Result: {}/{} passed ({:.1}%), {:.2}s average, {} tokens total ({} in / {} out)",
        totals.passed,
        totals.runs,
        totals.passed as f64 * 100.0 / totals.runs as f64,
        average,
        totals.total_tokens,
        totals.input_tokens,
        totals.output_tokens
    );
    if !totals.cache_hits.is_empty() {
        let aggregate = if totals.input_tokens > 0 {
            totals.cached_tokens as f64 / totals.input_tokens as f64 * 100.0
        } else {
            0.0
        };
        totals
            .cache_hits
            .sort_by(|left, right| left.partial_cmp(right).expect("cache ratios are finite"));
        let measured = totals.cache_hits.len();
        let median = if measured.is_multiple_of(2) {
            (totals.cache_hits[measured / 2 - 1] + totals.cache_hits[measured / 2]) / 2.0
        } else {
            totals.cache_hits[measured / 2]
        };
        let threshold = |minimum: f64| {
            totals
                .cache_hits
                .iter()
                .filter(|hit| **hit >= minimum)
                .count() as f64
                / measured as f64
                * 100.0
        };
        println!(
            "Prompt cache: median {:.0}% | aggregate {:.0}% | >=90%: {:.0}% | >=95%: {:.0}% | measured: {} | warm-up: {}",
            median,
            aggregate.min(100.0),
            threshold(90.0),
            threshold(95.0),
            measured,
            totals.warmups
        );
    }

    if totals.passed == totals.runs {
        Ok(())
    } else {
        anyhow::bail!("{} benchmark run(s) failed", totals.runs - totals.passed)
    }
}

fn load_suite(path: &Path) -> Result<Suite> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read benchmark suite {}", path.display()))?;
    let suite: Suite = serde_json::from_str(&content)
        .with_context(|| format!("invalid benchmark suite {}", path.display()))?;
    validate_suite(&suite)?;
    Ok(suite)
}

fn validate_suite(suite: &Suite) -> Result<()> {
    if suite.cases.is_empty() {
        anyhow::bail!("benchmark suite must contain at least one case");
    }
    for case in &suite.cases {
        if case.name.trim().is_empty() {
            anyhow::bail!("benchmark case name cannot be empty");
        }
        if case.prompt.trim().is_empty() {
            anyhow::bail!("benchmark case '{}' has an empty prompt", case.name);
        }
        if case
            .follow_up_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.trim().is_empty())
        {
            anyhow::bail!(
                "benchmark case '{}' has an empty follow_up_prompt",
                case.name
            );
        }
        if case
            .expect_contains
            .iter()
            .any(|expected| expected.trim().is_empty())
        {
            anyhow::bail!(
                "benchmark case '{}' has an empty expect_contains value",
                case.name
            );
        }
    }
    Ok(())
}

fn missing_expectations<'a>(content: &str, expected: &'a [String]) -> Vec<&'a str> {
    let content = content.to_lowercase();
    expected
        .iter()
        .filter(|needle| !content.contains(&needle.to_lowercase()))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expectations_are_case_insensitive() {
        let expected = vec!["Rust".to_string(), "ownership".to_string()];
        assert!(missing_expectations("RUST has Ownership", &expected).is_empty());
    }

    #[test]
    fn reports_every_missing_expectation() {
        let expected = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(missing_expectations("alpha", &expected), vec!["beta"]);
    }

    #[test]
    fn rejects_empty_suites_and_prompts() {
        assert!(validate_suite(&Suite { cases: Vec::new() }).is_err());
        assert!(
            validate_suite(&Suite {
                cases: vec![Case {
                    name: "empty".to_string(),
                    prompt: "  ".to_string(),
                    follow_up_prompt: None,
                    expect_contains: Vec::new(),
                }],
            })
            .is_err()
        );
    }

    #[test]
    fn follow_up_prompt_is_used_after_the_first_run() {
        let case = Case {
            name: "cache".into(),
            prompt: "long initial context".into(),
            follow_up_prompt: Some("short follow-up".into()),
            expect_contains: Vec::new(),
        };
        assert_eq!(case.prompt_for_run(1), "long initial context");
        assert_eq!(case.prompt_for_run(2), "short follow-up");
        assert_eq!(case.prompt_for_run(9), "short follow-up");
    }

    #[test]
    fn legacy_cases_repeat_the_initial_prompt() {
        let suite: Suite =
            serde_json::from_str(r#"{"cases":[{"name":"legacy","prompt":"repeat me"}]}"#).unwrap();
        assert_eq!(suite.cases[0].prompt_for_run(2), "repeat me");
    }
}
