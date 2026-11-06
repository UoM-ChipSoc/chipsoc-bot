//! `cargo run -p chipsoc-config --example check -- <config dir>`
fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "config".into());
    let r = chipsoc_config::authoring::check(std::path::Path::new(&dir))?;
    for e in &r.errors {
        println!("error: {e}");
    }
    for w in &r.warnings {
        println!("warning: {w}");
    }
    let c = chipsoc_config::Config::load(std::path::Path::new(&dir))?;
    for cf in &c.desired.categories {
        println!(
            "{} [{}]",
            cf.category.name,
            cf.channels.iter().map(|c| c.key.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}
