use std::path::Path;

use anyhow::{Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, Password, theme::ColorfulTheme};

use crate::{config, provider::openai::OpenAIProvider};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub async fn run(path: &Path) -> Result<()> {
    config::ensure_onboarding_supported(path)?;
    let theme = ColorfulTheme::default();

    println!("Kamui onboarding");
    println!("================");
    println!();
    println!("Connect Orvix Coding or another OpenAI-compatible provider.");

    loop {
        let provider = FuzzySelect::with_theme(&theme)
            .with_prompt("Provider profile")
            .items(["Orvix Coding", "Other OpenAI-compatible"])
            .default(0)
            .interact()?;
        let orvix_coding = provider == 0;
        let base_url = if orvix_coding {
            config::ORVIX_BASE_URL.to_owned()
        } else {
            Input::<String>::with_theme(&theme)
                .with_prompt("Provider base URL")
                .default(DEFAULT_BASE_URL.to_owned())
                .interact_text()?
                .trim_end_matches('/')
                .to_owned()
        };
        let api_key = Password::with_theme(&theme)
            .with_prompt("API key")
            .interact()?
            .trim()
            .to_owned();

        println!("Checking available models...");
        let models_path = orvix_coding.then_some(config::ORVIX_MODELS_PATH);
        match OpenAIProvider::list_models(&api_key, &base_url, models_path).await {
            // A provider can answer successfully with an empty list -- a key with no model
            // entitlements, or a base URL pointing at something that is not a model API.
            // `FuzzySelect` over no items has nothing to return, and indexing the empty list
            // afterwards would panic, on a first run, before anything else exists.
            Ok(models) if models.is_empty() => {
                eprintln!(
                    "That provider returned no models. Check the base URL and that the key has                      access to at least one model."
                );
                if !Confirm::with_theme(&theme)
                    .with_prompt("Try provider setup again?")
                    .default(true)
                    .interact()?
                {
                    bail!("provider setup cancelled");
                }
            }
            Ok(models) => {
                let selected = FuzzySelect::with_theme(&theme)
                    .with_prompt("Choose the default model (type to search)")
                    .items(&models)
                    .default(0)
                    .interact()?;
                config::save_onboarding(
                    path,
                    &base_url,
                    &api_key,
                    &models[selected],
                    orvix_coding,
                )?;
                println!("Connected. Found {} models.", models.len());
                println!("Configuration saved to {}", path.display());
                println!();
                return Ok(());
            }
            Err(error) => {
                eprintln!("Could not load models: {error:#}");
                if !Confirm::with_theme(&theme)
                    .with_prompt("Try provider setup again?")
                    .default(true)
                    .interact()?
                {
                    bail!("provider setup cancelled");
                }
            }
        }
    }
}
