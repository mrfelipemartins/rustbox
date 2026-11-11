use crate::error::{Error, Result};
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};
use serde_json::{Value, json};

fn addresses(value: Option<&Address<'_>>) -> Vec<Value> {
    value
        .into_iter()
        .flat_map(|v| v.iter())
        .map(|a| json!({"name":a.name,"address":a.address}))
        .collect()
}

fn identifiers(value: &HeaderValue<'_>) -> Vec<String> {
    if let Some(values) = value.as_text_list() {
        values
            .iter()
            .map(|v| format!("<{}>", v.trim_matches(['<', '>'])))
            .collect()
    } else {
        value
            .as_text()
            .map(|v| vec![format!("<{}>", v.trim_matches(['<', '>']))])
            .unwrap_or_default()
    }
}

pub fn parse(raw: &[u8]) -> Result<Value> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Invalid("invalid MIME message".into()))?;
    let headers: Vec<Value> = message
        .headers_raw()
        .map(|(name, value)| json!({"name":name,"value":value}))
        .collect();
    let classifications: Vec<Value> = headers
        .iter()
        .filter(|h| {
            let name = h["name"].as_str().unwrap_or_default().to_ascii_lowercase();
            matches!(
                name.as_str(),
                "authentication-results"
                    | "arc-authentication-results"
                    | "received-spf"
                    | "auto-submitted"
                    | "precedence"
                    | "content-type"
                    | "return-path"
                    | "x-failed-recipients"
                    | "x-auto-response-suppress"
                    | "x-autoreply"
                    | "x-autorespond"
                    | "disposition-notification-to"
                    | "return-receipt-to"
                    | "feedback-id"
            ) || name.starts_with("list-")
                || name.starts_with("x-spam")
                || name.starts_with("x-microsoft-antispam")
                || name.starts_with("x-forefront-antispam")
        })
        .cloned()
        .collect();
    let attachments: Vec<Value> = message.attachments().enumerate().map(|(index,part)| {
        let mime = part.content_type().map(|c| format!("{}/{}",c.ctype(),c.subtype().unwrap_or("octet-stream"))).unwrap_or_else(||"application/octet-stream".into());
        json!({"index":index,"filename":part.attachment_name(),"size":part.contents().len(),"content_type":mime,"disposition":part.content_disposition().map(|c|c.ctype()),"content_id":part.content_id()})
    }).collect();
    Ok(json!({
        "subject":message.subject(),
        "message_id":message.message_id().map(|v|format!("<{}>",v.trim_matches(['<','>']))),
        "in_reply_to":identifiers(message.in_reply_to()),"references":identifiers(message.references()),
        "from":addresses(message.from()),"sender":addresses(message.sender()),"reply_to":addresses(message.reply_to()),
        "to":addresses(message.to()),"cc":addresses(message.cc()),"bcc":addresses(message.bcc()),
        "date":message.date().map(|d|d.to_rfc3339()),
        "text":message.body_text(0),"html":message.body_html(0),
        "attachments":attachments,"classification_headers":classifications,"raw_headers":headers
    }))
}

pub fn metadata(raw: &[u8]) -> Result<Value> {
    let mut value = parse(raw)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("text");
        object.remove("html");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_headers_preserve_original_values_and_missing_recipients() {
        let parsed=parse(b"Message-ID: <one@test>\r\nIn-Reply-To: <parent@test>\r\nReferences: <first@test> <parent@test>\r\nFrom: Person <person@test>\r\nSender: Agent <agent@test>\r\nReply-To: replies@test\r\nTo: other@test\r\nAuto-Submitted: auto-generated\r\nDate: Tue, 10 Nov 2026 10:00:00 +0000\r\n\r\nHello").unwrap();
        assert_eq!(parsed["message_id"], "<one@test>");
        assert_eq!(
            parsed["references"],
            json!(["<first@test>", "<parent@test>"])
        );
        assert_eq!(parsed["sender"][0]["address"], "agent@test");
        assert_eq!(parsed["bcc"], json!([]));
        assert_eq!(
            parsed["classification_headers"][0]["name"],
            "Auto-Submitted"
        );
    }
}
