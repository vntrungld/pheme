//! Implemented in a later task.
use crate::config::Config;

pub async fn main(_cfg: Config, _host: Option<&str>, _stats: bool) -> anyhow::Result<()> {
    anyhow::bail!("not implemented yet")
}

pub async fn pair(_cfg: Config, _host: &str, _code: &str) -> anyhow::Result<()> {
    anyhow::bail!("not implemented yet")
}
