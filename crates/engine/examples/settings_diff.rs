//! Debug aid: print config vs Discord settings that differ.
//! `cargo run -p chipsoc-engine --example settings_diff -- config`
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let dir = std::env::args().nth(1).unwrap_or_else(|| "config".into());
    let e = chipsoc_engine::Engine::load(chipsoc_discord::Http::new(std::env::var("DISCORD_TOKEN")?), dir.as_ref())?;
    let live = e.live().await?;
    let want = &e.cfg.desired;
    if let Some(o) = &want.settings.onboarding {
        let w = chipsoc_config::resolved_onboarding(o, want);
        println!("--- onboarding want\n{}", toml::to_string_pretty(&w)?);
        println!("--- onboarding live\n{}", toml::to_string_pretty(&live.settings.onboarding)?);
    }
    for r in &want.settings.automod {
        if let Some(l) = live.settings.automod.iter().find(|x| x.name == r.name)
            && l != r
        {
            println!("--- automod want\n{}\n--- live\n{}", toml::to_string_pretty(r)?, toml::to_string_pretty(l)?);
        }
    }
    Ok(())
}
