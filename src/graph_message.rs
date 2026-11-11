use crate::error::{Error, Result};
use mail_parser::{Encoding, HeaderName, Message, MessageParser, MimeHeaders, PartType};
use std::collections::HashSet;

pub const MAX_DRAFT_MIME_BYTES: usize = 3_000_000;
pub const MAX_ATTACHMENT_BYTES: usize = 150_000_000;
pub const SMALL_ATTACHMENT_BYTES: usize = 3_000_000;

pub struct GraphMessage {
    pub base_mime: Vec<u8>,
    pub attachments: Vec<GraphAttachment>,
}

#[derive(Clone)]
pub struct GraphAttachment {
    pub name: String,
    pub content_type: String,
    pub content_id: Option<String>,
    pub is_inline: bool,
    pub data: Vec<u8>,
}

impl GraphMessage {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let mut message = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| Error::Invalid("invalid Graph MIME message".into()))?;
        let removed: HashSet<_> = message.attachments.iter().copied().collect();
        let base_mime = render_part(&message, raw, 0, &removed, 0)?
            .ok_or_else(|| Error::Invalid("Graph draft has no message body".into()))?;
        validate_draft_size(base_mime.len())?;
        let mut attachments = Vec::with_capacity(removed.len());
        for (index, id) in message.attachments.clone().into_iter().enumerate() {
            let part = message
                .parts
                .get_mut(id as usize)
                .ok_or_else(|| Error::Invalid("invalid MIME attachment".into()))?;
            if part.is_encoding_problem {
                return Err(Error::Invalid("invalid MIME attachment encoding".into()));
            }
            let mut attachment = GraphAttachment {
                name: part
                    .attachment_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("attachment-{}", index + 1)),
                content_type: part
                    .headers
                    .iter()
                    .find(|header| header.name == HeaderName::ContentType)
                    .and_then(|header| {
                        raw.get(header.offset_start as usize..header.offset_end as usize)
                    })
                    .and_then(|value| std::str::from_utf8(value).ok())
                    .map(|value| value.lines().map(str::trim).collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|| "application/octet-stream".into()),
                content_id: part
                    .content_id()
                    .map(|value| value.trim_matches(['<', '>']).to_owned()),
                is_inline: part
                    .content_disposition()
                    .is_some_and(|value| value.ctype().eq_ignore_ascii_case("inline"))
                    || matches!(part.body, PartType::InlineBinary(_)),
                data: Vec::new(),
            };
            let contents = match std::mem::take(&mut part.body) {
                PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.into_owned(),
                _ => {
                    let encoded = raw
                        .get(part.offset_body as usize..part.offset_end as usize)
                        .ok_or_else(|| Error::Invalid("invalid MIME attachment offsets".into()))?;
                    match part.encoding {
                        Encoding::Base64 => mail_parser::decoders::base64::base64_decode(encoded),
                        Encoding::QuotedPrintable => {
                            mail_parser::decoders::quoted_printable::quoted_printable_decode(
                                encoded,
                            )
                        }
                        Encoding::None => Some(encoded.to_vec()),
                    }
                    .ok_or_else(|| Error::Invalid("invalid MIME attachment encoding".into()))?
                }
            };
            validate_attachment_size(contents.len())?;
            attachment.data = contents;
            attachments.push(attachment);
        }
        Ok(Self {
            base_mime,
            attachments,
        })
    }
}

pub fn validate_attachment_size(size: usize) -> Result<()> {
    if size > MAX_ATTACHMENT_BYTES {
        return Err(Error::SendComponentLimitExceeded {
            backend: "microsoft",
            component: "decoded_attachment",
            maximum_bytes: MAX_ATTACHMENT_BYTES,
            actual_bytes: size,
        });
    }
    Ok(())
}

pub fn validate_draft_size(size: usize) -> Result<()> {
    if size > MAX_DRAFT_MIME_BYTES {
        return Err(Error::SendComponentLimitExceeded {
            backend: "microsoft",
            component: "draft_mime",
            maximum_bytes: MAX_DRAFT_MIME_BYTES,
            actual_bytes: size,
        });
    }
    Ok(())
}

fn render_part(
    message: &Message<'_>,
    raw: &[u8],
    id: u32,
    removed: &HashSet<u32>,
    depth: usize,
) -> Result<Option<Vec<u8>>> {
    if removed.contains(&id) {
        return Ok(None);
    }
    if depth > 32 {
        return Err(Error::Invalid(
            "MIME nesting exceeds the Graph preparation limit".into(),
        ));
    }
    let part = message
        .parts
        .get(id as usize)
        .ok_or_else(|| Error::Invalid("invalid MIME structure".into()))?;
    match &part.body {
        PartType::Multipart(children) => {
            let boundary = part
                .content_type()
                .and_then(|value| value.attribute("boundary"))
                .filter(|value| !value.is_empty() && !value.contains(['\r', '\n']))
                .ok_or_else(|| Error::Invalid("MIME multipart boundary missing".into()))?;
            let mut rendered = Vec::new();
            for child in children {
                if let Some(child) = render_part(message, raw, *child, removed, depth + 1)? {
                    rendered.push(child);
                }
            }
            if rendered.is_empty() {
                return Ok(None);
            }
            let mut result = raw
                .get(part.offset_header as usize..part.offset_body as usize)
                .ok_or_else(|| Error::Invalid("invalid MIME header offsets".into()))?
                .to_vec();
            for child in rendered {
                result.extend_from_slice(b"--");
                result.extend_from_slice(boundary.as_bytes());
                result.extend_from_slice(b"\r\n");
                result.extend_from_slice(&child);
                if !result.ends_with(b"\r\n") {
                    result.extend_from_slice(b"\r\n");
                }
            }
            result.extend_from_slice(b"--");
            result.extend_from_slice(boundary.as_bytes());
            result.extend_from_slice(b"--\r\n");
            Ok(Some(result))
        }
        _ => {
            if part.is_encoding_problem {
                return Err(Error::Invalid("invalid MIME body encoding".into()));
            }
            let bytes = raw
                .get(part.offset_header as usize..part.offset_end as usize)
                .ok_or_else(|| Error::Invalid("invalid MIME body offsets".into()))?;
            validate_draft_size(bytes.len())?;
            Ok(Some(bytes.to_vec()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lettre::{
        Message as Mail,
        message::{Attachment, MultiPart, SinglePart, header::ContentType},
    };

    #[test]
    fn preparation_preserves_headers_bodies_and_attachment_attributes() {
        let original = Mail::builder()
            .from("Sender <sender@example.com>".parse().unwrap())
            .to("recipient@example.com".parse().unwrap())
            .reply_to("replies@example.com".parse().unwrap())
            .message_id(Some("<graph-message@example.com>".into()))
            .in_reply_to("<parent@example.com>".into())
            .references("<first@example.com> <parent@example.com>".into())
            .subject("Unicode café")
            .multipart(
                MultiPart::mixed()
                    .multipart(
                        MultiPart::related()
                            .multipart(
                                MultiPart::alternative()
                                    .singlepart(SinglePart::plain(
                                        "Plain café\r\nSecond line".to_owned(),
                                    ))
                                    .singlepart(SinglePart::html(
                                        "<p>HTML café</p><img src=\"cid:logo@example.com\">"
                                            .to_owned(),
                                    )),
                            )
                            .singlepart(
                                Attachment::new_inline("logo@example.com".into()).body(
                                    vec![0, 1, 2, 3],
                                    ContentType::parse("image/png").unwrap(),
                                ),
                            ),
                    )
                    .singlepart(
                        Attachment::new("notes.txt".into())
                            .body(b"attachment contents".to_vec(), ContentType::TEXT_PLAIN),
                    ),
            )
            .unwrap()
            .formatted();
        let mut raw = b"Bcc: hidden@example.com\r\nSender: delegate@example.com\r\n".to_vec();
        raw.extend_from_slice(&original);
        let prepared = GraphMessage::parse(&raw).unwrap();
        let before = MessageParser::default().parse(&raw).unwrap();
        let after = MessageParser::default().parse(&prepared.base_mime).unwrap();
        assert_eq!(after.message_id(), before.message_id());
        assert_eq!(after.in_reply_to(), before.in_reply_to());
        assert_eq!(after.references(), before.references());
        assert_eq!(after.from(), before.from());
        assert_eq!(after.sender(), before.sender());
        assert_eq!(after.reply_to(), before.reply_to());
        assert_eq!(after.to(), before.to());
        assert_eq!(after.bcc(), before.bcc());
        assert_eq!(after.subject(), before.subject());
        assert_eq!(after.body_text(0), before.body_text(0));
        assert_eq!(after.body_html(0), before.body_html(0));
        assert_eq!(after.attachments().count(), 0);
        assert_eq!(prepared.attachments.len(), 2);
        assert_eq!(prepared.attachments[0].content_type, "image/png");
        assert_eq!(
            prepared.attachments[0].content_id.as_deref(),
            Some("logo@example.com")
        );
        assert!(prepared.attachments[0].is_inline);
        assert_eq!(prepared.attachments[0].data, vec![0, 1, 2, 3]);
        assert_eq!(prepared.attachments[1].name, "notes.txt");
        assert!(!prepared.attachments[1].is_inline);
        assert_eq!(prepared.attachments[1].data, b"attachment contents");
    }

    #[test]
    fn large_inline_and_regular_files_are_removed_from_the_base_draft() {
        let regular = vec![7u8; SMALL_ATTACHMENT_BYTES + 1];
        let inline = vec![9u8; SMALL_ATTACHMENT_BYTES + 1024];
        let raw = Mail::builder()
            .from("sender@example.com".parse().unwrap())
            .to("other@example.com".parse().unwrap())
            .multipart(
                MultiPart::mixed()
                    .multipart(
                        MultiPart::related()
                            .singlepart(SinglePart::html(
                                "<img src=\"cid:large@example.com\">".to_owned(),
                            ))
                            .singlepart(
                                Attachment::new_inline_with_name(
                                    "large@example.com".into(),
                                    "large.png".into(),
                                )
                                .body(inline.clone(), ContentType::parse("image/png").unwrap()),
                            ),
                    )
                    .singlepart(Attachment::new("large.bin".into()).body(
                        regular.clone(),
                        ContentType::parse("application/octet-stream").unwrap(),
                    )),
            )
            .unwrap()
            .formatted();
        assert!(raw.len() > 6_000_000);
        let prepared = GraphMessage::parse(&raw).unwrap();
        assert!(prepared.base_mime.len() < 8192);
        assert_eq!(prepared.attachments.len(), 2);
        assert_eq!(prepared.attachments[0].data, inline);
        assert_eq!(
            prepared.attachments[0].content_id.as_deref(),
            Some("large@example.com")
        );
        assert_eq!(prepared.attachments[0].name, "large.png");
        assert!(prepared.attachments[0].is_inline);
        assert_eq!(prepared.attachments[1].data, regular);
        assert_eq!(prepared.attachments[1].name, "large.bin");
        let draft = MessageParser::default().parse(&prepared.base_mime).unwrap();
        assert_eq!(draft.attachments().count(), 0);
        assert_eq!(
            draft.body_html(0).as_deref(),
            Some("<img src=\"cid:large@example.com\">")
        );
    }

    #[test]
    fn text_attachments_retain_original_bytes_and_charset() {
        for (encoding, body) in [("base64", "Y2Fm6Q=="), ("quoted-printable", "caf=E9")] {
            let raw = format!(
                "From: sender@example.com\r\nTo: recipient@example.com\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=test\r\n\r\n--test\r\nContent-Type: text/plain\r\n\r\nBody\r\n--test\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Disposition: attachment; filename=cafe.txt\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n{body}\r\n--test--\r\n"
            );
            let prepared = GraphMessage::parse(raw.as_bytes()).unwrap();
            assert_eq!(prepared.attachments.len(), 1);
            assert_eq!(prepared.attachments[0].data, b"caf\xe9");
            assert_eq!(
                prepared.attachments[0].content_type,
                "text/plain; charset=iso-8859-1"
            );
        }
    }

    #[test]
    fn attachment_limits_are_decoded_and_draft_limit_is_independent() {
        assert!(validate_attachment_size(MAX_ATTACHMENT_BYTES).is_ok());
        assert!(validate_attachment_size(MAX_ATTACHMENT_BYTES + 1).is_err());
        assert!(validate_draft_size(MAX_DRAFT_MIME_BYTES).is_ok());
        assert!(validate_draft_size(MAX_DRAFT_MIME_BYTES + 1).is_err());
        let mut raw = b"From: sender@example.com\r\nTo: other@example.com\r\n\r\n".to_vec();
        raw.resize(MAX_DRAFT_MIME_BYTES + 1, b'x');
        assert!(GraphMessage::parse(&raw).is_err());
    }

    #[test]
    fn single_part_and_empty_alternative_branches_remain_valid() {
        let raw=b"From: sender@example.com\r\nTo: other@example.com\r\nMessage-ID: <a@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello";
        let prepared = GraphMessage::parse(raw).unwrap();
        assert_eq!(prepared.base_mime, raw);
        assert!(prepared.attachments.is_empty());
    }
}
