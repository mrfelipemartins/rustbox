use rustbox::db::{AccountInput, MailAuth};
use serde_json::json;

fn account() -> AccountInput {
    serde_json::from_value(json!({
        "name":"compatibility",
        "imap":{"host":"imap.gmail.com","port":993,"username":"person@example.com","password":"secret"},
        "smtp":{"host":"smtp.gmail.com","port":465,"username":"person@example.com","password":"secret"}
    })).unwrap()
}

#[test]
fn legacy_password_configuration_remains_valid() {
    let input = account();
    input.validate().unwrap();
    assert!(input.imap.oauth_credential().is_none());
}

#[test]
fn oauth_references_are_unambiguous_and_sasl_usernames_are_safe() {
    let mut input = account();
    let id = uuid::Uuid::new_v4().to_string();
    input.imap.auth = Some(MailAuth::Oauth2 {
        credential_id: id.clone(),
    });
    assert!(input.validate().is_err());
    input.imap.password.clear();
    input.validate().unwrap();
    assert_eq!(input.imap.oauth_credential(), Some(id.as_str()));
    input.imap.username = "user\x01auth=Bearer attacker".into();
    assert!(input.validate().is_err());
}

#[test]
fn oauth_configuration_round_trips_without_password_field() {
    let mut input = account();
    input.smtp.password.clear();
    input.smtp.auth = Some(MailAuth::Oauth2 {
        credential_id: uuid::Uuid::new_v4().to_string(),
    });
    let value = serde_json::to_value(&input).unwrap();
    assert!(value["smtp"].get("password").is_none());
    let restored: AccountInput = serde_json::from_value(value).unwrap();
    restored.validate().unwrap();
}
