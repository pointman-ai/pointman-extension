//! A sign-in with Pointman's shared OAuth client: a form whose `shared_client` names the provider gets the client from
//! the node ([`Handle::oauth_shared_client`]), and exchanges ([`Handle::oauth_token`]) and refreshes
//! ([`Handle::oauth_refresh`]) through core, so the client's secret never reaches it. `credentials.sign_in` gives the
//! `redirect_uri` to send people back to, and `credentials.code` the code the redirect brought. Core's refusal is a
//! [`HostError`] with core's code: `invalid_grant` means the person signs in again, `upstream` that it's worth trying
//! later. Never log a code, a verifier or a token.

use anyhow::Context;
use pointman_extension::{Extension, Handle, HostError, Job};
use serde_json::{json, Value};

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let h = ext.handle();
    ext.command("signin.try", move |job: &Job| tried(&h, &job.params));
    ext.run()
}

/// What the test asks of the node, and its answer, or the node's refusal with core's code.
fn tried(h: &Handle, p: &Value) -> anyhow::Result<Value> {
    let provider = p["provider"].as_str().context("provider is missing")?;
    let text = |k: &str| p[k].as_str().unwrap_or_default();
    let got = match p["what"].as_str() {
        Some("client") => h.oauth_shared_client(provider).map(|c| json!(c)),
        Some("code") => h.oauth_token(provider, text("code"), text("verifier"), text("redirect_uri")).map(|t| json!(t)),
        Some("refresh") => h.oauth_refresh(provider, text("refresh_token")).map(|t| json!(t)),
        _ => anyhow::bail!("what: client, code or refresh"),
    };
    match got {
        Ok(answer) => Ok(json!({"got": answer})),
        Err(e) => match e.downcast_ref::<HostError>() {
            Some(refused) => Ok(json!({"refused": refused.message, "code": refused.code})),
            None => Err(e),
        },
    }
}
