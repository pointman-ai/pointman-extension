//! A sign-in with Pointman's shared OAuth client, through the signin example under the stand-in node: the client and
//! the tokens come from the node, core's refusal keeps its code, and a provider no sign-in form names is refused
//! before anything is asked, as on the node.

mod common;

use common::{example, source};
use pointman_extension::protocol::{
    oauth_token_request::Grant, OauthSharedClientRequest, OauthSharedClientResponse, OauthTokenRequest,
    OauthTokenResponse,
};
use pointman_extension::testing::StandIn;
use pointman_extension::HostError;
use serde_json::{json, Value};

const CLOUD: &str = "https://api.pointman.ai/v1/oauth/google/callback";

/// Core's shared client and its broker, as the extension sees them: Google's answer, or core's refusal with its code.
fn signin() -> StandIn {
    StandIn::new(source("signin"))
        .binary(example("signin"))
        .on(|_: OauthSharedClientRequest| {
            Ok(OauthSharedClientResponse {
                shared: true,
                client_id: "1234-abc.apps.googleusercontent.com".into(),
                redirect_uri: CLOUD.into(),
                ..Default::default()
            })
        })
        .on(|r: OauthTokenRequest| match r.grant {
            Some(Grant::Code(c)) if c.code == "c-ok" && c.verifier == "v-1" && c.redirect_uri == CLOUD => {
                Ok(OauthTokenResponse {
                    access_token: "at-1".into(),
                    expires_in: 3599,
                    token_type: "Bearer".into(),
                    scope: "calendar".into(),
                    refresh_token: Some("rt-1".into()),
                    id_token: Some("idt-1".into()),
                    ..Default::default()
                })
            }
            Some(Grant::RefreshToken(t)) if t == "rt-later" => {
                Err(HostError::with_code("api.pointman.ai didn't answer", "upstream").into())
            }
            Some(Grant::RefreshToken(_)) => Ok(OauthTokenResponse {
                access_token: "at-2".into(),
                expires_in: 3599,
                token_type: "Bearer".into(),
                scope: "calendar".into(),
                ..Default::default()
            }),
            _ => Err(HostError::with_code("Bad Request", "invalid_grant").into()),
        })
}

fn tried(node: &StandIn, params: Value) -> Value {
    node.run("signin.try", params).unwrap().result
}

#[test]
fn the_shared_client_and_its_tokens_come_through_the_node() {
    let node = signin().start().unwrap();
    assert_eq!(
        tried(&node, json!({"what": "client", "provider": "google"})),
        json!({"got": {"shared": true, "clientId": "1234-abc.apps.googleusercontent.com", "redirectUri": CLOUD}})
    );
    let code = json!({"what": "code", "provider": "google", "code": "c-ok", "verifier": "v-1", "redirect_uri": CLOUD});
    assert_eq!(
        tried(&node, code),
        json!({"got": {"accessToken": "at-1", "expiresIn": "3599", "tokenType": "Bearer", "scope": "calendar",
                       "refreshToken": "rt-1", "idToken": "idt-1"}})
    );
    assert_eq!(
        tried(&node, json!({"what": "refresh", "provider": "google", "refresh_token": "rt-1"})),
        json!({"got": {"accessToken": "at-2", "expiresIn": "3599", "tokenType": "Bearer", "scope": "calendar"}})
    );
    let asked: Vec<(String, Value)> = node.asked().into_iter().filter(|(m, _)| m.starts_with("oauth.")).collect();
    assert_eq!(
        asked,
        [
            ("oauth.shared_client".to_string(), json!({"provider": "google"})),
            (
                "oauth.token".to_string(),
                json!({"provider": "google", "code": {"code": "c-ok", "verifier": "v-1", "redirectUri": CLOUD}})
            ),
            ("oauth.token".to_string(), json!({"provider": "google", "refreshToken": "rt-1"})),
        ]
    );
}

#[test]
fn cores_refusal_keeps_its_code() {
    let node = signin().start().unwrap();
    let used = json!({"what": "code", "provider": "google", "code": "c-used", "verifier": "v-1",
                      "redirect_uri": CLOUD});
    assert_eq!(tried(&node, used), json!({"refused": "Bad Request", "code": "invalid_grant"}));
    assert_eq!(
        tried(&node, json!({"what": "refresh", "provider": "google", "refresh_token": "rt-later"})),
        json!({"refused": "api.pointman.ai didn't answer", "code": "upstream"})
    );
}

#[test]
fn a_provider_no_form_names_is_refused_before_anything_is_asked() {
    let node = signin().start().unwrap();
    let said = "signin may not use a shared \"github\" client: none of its sign-in forms names it";
    for ask in [
        json!({"what": "client", "provider": "github"}),
        json!({"what": "refresh", "provider": "github", "refresh_token": "rt-1"}),
    ] {
        assert_eq!(tried(&node, ask), json!({"refused": said, "code": null}));
    }
}

#[test]
fn a_host_error_is_the_json_rpc_error_a_node_sends() {
    let refused = HostError::with_code("Bad Request", "invalid_grant");
    let wire = json!({"code": -32000, "message": "Bad Request", "data": {"code": "invalid_grant"}});
    assert_eq!(refused.to_error(), wire);
    assert_eq!(HostError::from_error(&wire), refused);
    // an older node's error has no data: no code
    assert_eq!(HostError::from_error(&json!({"code": -32000, "message": "no"})), HostError::new("no"));
}
