use std::collections::HashMap;

use async_imap::{Client, Session};
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{net::TcpStream, time::Instant};
use tokio_rustls::client::TlsStream;

use crate::{
    config::Config,
    db::{Account, Db},
    error::{Error, Result},
    security,
};

#[derive(sqlx::FromRow)]
struct Checkpoint {
    uidvalidity: u32,
    last_uid: u32,
    highest_modseq: Option<u64>,
    initial_uid_boundary: u32,
    backfill_complete: bool,
}

pub type MailSession = Session<TlsStream<TcpStream>>;

pub struct Connection {
    pub session: MailSession,
    pub idle: bool,
    pub condstore: bool,
    pub move_supported: bool,
    pub uidplus: bool,
    status: HashMap<String, String>,
    next_index_check: HashMap<String, Instant>,
}

pub enum ConnectFailure {
    Authentication,
    Permission,
    Other(Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub name: String,
    pub delimiter: Option<String>,
    pub attributes: Vec<String>,
}

#[derive(Debug)]
pub enum Command {
    Sync,
    ListFolders,
    SearchMessageId {
        mailbox: String,
        message_id: String,
    },
    Content {
        mailbox: String,
        uidvalidity: u32,
        uid: u32,
    },
    Flags {
        mailbox: String,
        uidvalidity: u32,
        uid: u32,
        flags: Vec<String>,
    },
    Move {
        mailbox: String,
        uidvalidity: u32,
        uid: u32,
        destination: String,
    },
    Trash {
        mailbox: String,
        uidvalidity: u32,
        uid: u32,
    },
    Append {
        mailbox: String,
        raw: Vec<u8>,
        expected_target: Option<Value>,
    },
}

impl Command {
    pub fn mutating(&self) -> bool {
        matches!(
            self,
            Self::Flags { .. } | Self::Move { .. } | Self::Trash { .. } | Self::Append { .. }
        )
    }
}

pub enum Reply {
    Folders(Vec<Folder>),
    Matches { uidvalidity: u32, uids: Vec<u32> },
    Content(Vec<u8>),
    Done,
}

fn protocol(error: async_imap::error::Error) -> Error {
    match error {
        async_imap::error::Error::No(_) => {
            Error::Invalid("mail server rejected the command".into())
        }
        async_imap::error::Error::Bad(_) | async_imap::error::Error::Validate(_) => {
            Error::Invalid("mail server rejected command syntax".into())
        }
        _ => Error::Unavailable("IMAP connection failed".into()),
    }
}

pub async fn connect(
    account: &Account,
    config: &Config,
    oauth: &crate::oauth::OAuth,
) -> std::result::Result<Connection, ConnectFailure> {
    let server = &account.config.imap;
    let mut token = if let Some(id) = server.oauth_credential() {
        oauth
            .validate_mail_server(server, crate::oauth::MailProtocol::Imap)
            .await
            .map_err(ConnectFailure::Other)?;
        Some(
            oauth
                .token(id, false)
                .await
                .map_err(ConnectFailure::Other)?,
        )
    } else {
        None
    };
    let mut session = {
        let client = open_client(server, config).await?;
        let result = if let Some(token) = &token {
            client
                .authenticate("XOAUTH2", Xoauth2::new(&server.username, token))
                .await
        } else {
            client.login(&server.username, &server.password).await
        };
        match result {
            Ok(session) => session,
            Err((async_imap::error::Error::No(_), _)) if token.is_some() => {
                let Some(id) = server.oauth_credential() else {
                    return Err(ConnectFailure::Authentication);
                };
                token = Some(oauth.token(id, true).await.map_err(ConnectFailure::Other)?);
                let client = open_client(server, config).await?;
                match client
                    .authenticate(
                        "XOAUTH2",
                        Xoauth2::new(&server.username, token.as_deref().unwrap_or_default()),
                    )
                    .await
                {
                    Ok(session) => session,
                    Err((async_imap::error::Error::No(_), _)) => {
                        return Err(ConnectFailure::Permission);
                    }
                    Err((error, _)) => return Err(ConnectFailure::Other(protocol(error))),
                }
            }
            Err((async_imap::error::Error::No(_), _)) => {
                return Err(ConnectFailure::Authentication);
            }
            Err((error, _)) => return Err(ConnectFailure::Other(protocol(error))),
        }
    };
    let caps = session
        .capabilities()
        .await
        .map_err(|e| ConnectFailure::Other(protocol(e)))?;
    let idle = caps.has_str("IDLE");
    let condstore = caps.has_str("CONDSTORE");
    let move_supported = caps.has_str("MOVE");
    let uidplus = caps.has_str("UIDPLUS");
    session
        .select("INBOX")
        .await
        .map_err(|e| ConnectFailure::Other(protocol(e)))?;
    Ok(Connection {
        session,
        idle,
        condstore,
        move_supported,
        uidplus,
        status: HashMap::new(),
        next_index_check: HashMap::new(),
    })
}

struct Xoauth2 {
    response: Option<Vec<u8>>,
}

impl Xoauth2 {
    fn new(username: &str, token: &str) -> Self {
        Self {
            response: Some(format!("user={username}\x01auth=Bearer {token}\x01\x01").into_bytes()),
        }
    }
}

impl async_imap::Authenticator for Xoauth2 {
    type Response = Vec<u8>;
    fn process(&mut self, _: &[u8]) -> Self::Response {
        self.response.take().unwrap_or_default()
    }
}

async fn open_client(
    server: &crate::db::MailServer,
    config: &Config,
) -> std::result::Result<Client<TlsStream<TcpStream>>, ConnectFailure> {
    let tcp = security::connect(&server.host, server.port, config.allow_private_networks)
        .await
        .map_err(ConnectFailure::Other)?;
    let client = if server.tls_mode == "starttls" {
        let mut client = Client::new(tcp);
        client
            .read_response()
            .await
            .map_err(|e| ConnectFailure::Other(protocol(e.into())))?;
        client
            .run_command_and_check_ok("STARTTLS", None)
            .await
            .map_err(|e| ConnectFailure::Other(protocol(e)))?;
        Client::new(
            security::tls(
                client.into_inner(),
                &server.host,
                config.tls_ca_file.as_deref(),
            )
            .await
            .map_err(ConnectFailure::Other)?,
        )
    } else {
        let tls = security::tls(tcp, &server.host, config.tls_ca_file.as_deref())
            .await
            .map_err(ConnectFailure::Other)?;
        let mut client = Client::new(tls);
        client
            .read_response()
            .await
            .map_err(|e| ConnectFailure::Other(protocol(e.into())))?;
        client
    };
    Ok(client)
}

pub async fn execute(
    connection: &mut Connection,
    account: &Account,
    command: Command,
    config: &Config,
    db: &Db,
) -> Result<Reply> {
    match command {
        Command::Sync => {
            reconcile(connection, db, account, true, true).await?;
            Ok(Reply::Done)
        }
        Command::SearchMessageId {
            mailbox,
            message_id,
        } => {
            if message_id.is_empty() || message_id.contains(['\r', '\n', '\0', '"', '\\']) {
                return Err(Error::Invalid("invalid message identifier".into()));
            }
            let selected = connection
                .session
                .select(&mailbox)
                .await
                .map_err(protocol)?;
            let uidvalidity = selected
                .uid_validity
                .ok_or_else(|| Error::Unsupported("persistent UIDs are required".into()))?;
            let mut uids: Vec<u32> = connection
                .session
                .uid_search(format!("HEADER Message-ID \"{message_id}\""))
                .await
                .map_err(protocol)?
                .into_iter()
                .collect();
            uids.sort_unstable();
            uids.truncate(100);
            Ok(Reply::Matches { uidvalidity, uids })
        }
        Command::ListFolders => {
            let mut stream = connection
                .session
                .list(None, Some("*"))
                .await
                .map_err(protocol)?;
            let mut folders = Vec::new();
            while let Some(folder) = stream.try_next().await.map_err(protocol)? {
                folders.push(Folder {
                    name: folder.name().into(),
                    delimiter: folder.delimiter().map(str::to_owned),
                    attributes: folder
                        .attributes()
                        .iter()
                        .map(|a| format!("{a:?}"))
                        .collect(),
                });
            }
            Ok(Reply::Folders(folders))
        }
        Command::Append {
            mailbox,
            raw,
            expected_target,
        } => {
            if expected_target
                .is_some_and(|target| target != crate::smtp::append_target(&account.config.imap))
            {
                return Err(Error::Conflict(
                    "Sent-copy account configuration changed".into(),
                ));
            }
            connection
                .session
                .append(mailbox, Some("(\\Seen)"), None, raw)
                .await
                .map_err(mutation_error)?;
            Ok(Reply::Done)
        }
        Command::Content {
            mailbox,
            uidvalidity,
            uid,
        } => {
            verify_message(connection, &mailbox, uidvalidity, uid).await?;
            let size = {
                let mut stream = connection
                    .session
                    .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
                    .await
                    .map_err(protocol)?;
                let size = stream
                    .try_next()
                    .await
                    .map_err(protocol)?
                    .and_then(|f| f.size);
                while stream.try_next().await.map_err(protocol)?.is_some() {}
                size.ok_or(Error::NotFound)? as usize
            };
            if size > config.max_message_bytes {
                return Err(Error::Invalid("message exceeds content size limit".into()));
            }
            let mut bytes = Vec::with_capacity(size);
            while bytes.len() < size {
                let query = format!(
                    "(UID BODY.PEEK[]<{}.{}>)",
                    bytes.len(),
                    (size - bytes.len()).min(65536)
                );
                let mut stream = connection
                    .session
                    .uid_fetch(uid.to_string(), query)
                    .await
                    .map_err(protocol)?;
                let fetch = stream
                    .try_next()
                    .await
                    .map_err(protocol)?
                    .ok_or(Error::NotFound)?;
                let body = fetch.body().ok_or_else(|| {
                    Error::Unavailable("provider returned no message body".into())
                })?;
                if body.is_empty() || bytes.len() + body.len() > config.max_message_bytes {
                    return Err(Error::Unavailable(
                        "provider returned an invalid content length".into(),
                    ));
                }
                bytes.extend_from_slice(body);
                while stream.try_next().await.map_err(protocol)?.is_some() {}
            }
            Ok(Reply::Content(bytes))
        }
        Command::Flags {
            mailbox,
            uidvalidity,
            uid,
            flags,
        } => {
            if flags.iter().any(|flag| {
                !matches!(
                    flag.as_str(),
                    "\\Seen" | "\\Answered" | "\\Flagged" | "\\Draft"
                )
            }) {
                return Err(Error::Invalid(
                    "flags must be Seen, Answered, Flagged, or Draft IMAP system flags".into(),
                ));
            }
            verify_message(connection, &mailbox, uidvalidity, uid).await?;
            let query = format!("FLAGS ({})", flags.join(" "));
            let mut stream = connection
                .session
                .uid_store(uid.to_string(), query)
                .await
                .map_err(mutation_error)?;
            while stream.try_next().await.map_err(mutation_error)?.is_some() {}
            Ok(Reply::Done)
        }
        Command::Move {
            mailbox,
            uidvalidity,
            uid,
            destination,
        } => {
            let mapping =
                move_message(connection, &mailbox, uidvalidity, uid, &destination).await?;
            persist_move(
                db,
                account,
                &mailbox,
                uidvalidity,
                uid,
                &destination,
                mapping,
            )
            .await?;
            Ok(Reply::Done)
        }
        Command::Trash {
            mailbox,
            uidvalidity,
            uid,
        } => {
            let destination = if let Some(folder) = &account.config.trash_folder {
                folder.clone()
            } else {
                let mut stream = connection
                    .session
                    .list(None, Some("*"))
                    .await
                    .map_err(protocol)?;
                let mut found = None;
                while let Some(folder) = stream.try_next().await.map_err(protocol)? {
                    if folder
                        .attributes()
                        .iter()
                        .any(|a| format!("{a:?}").to_lowercase().contains("trash"))
                    {
                        found = Some(folder.name().to_owned());
                    }
                }
                found.ok_or_else(|| Error::Invalid("configure an existing trash folder".into()))?
            };
            let mapping =
                move_message(connection, &mailbox, uidvalidity, uid, &destination).await?;
            persist_move(
                db,
                account,
                &mailbox,
                uidvalidity,
                uid,
                &destination,
                mapping,
            )
            .await?;
            Ok(Reply::Done)
        }
    }
}

async fn verify_message(
    connection: &mut Connection,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
) -> Result<()> {
    if uid == 0 {
        return Err(Error::Invalid("UID must be positive".into()));
    }
    let selected = connection.session.select(mailbox).await.map_err(protocol)?;
    if selected.uid_validity != Some(uidvalidity) {
        return Err(Error::Conflict(
            "mailbox UIDVALIDITY changed; refresh metadata".into(),
        ));
    }
    let found = connection
        .session
        .uid_search(format!("UID {uid}"))
        .await
        .map_err(protocol)?;
    if !found.contains(&uid) {
        return Err(Error::NotFound);
    }
    Ok(())
}

fn mutation_error(error: async_imap::error::Error) -> Error {
    match protocol(error) {
        Error::Unavailable(_) => Error::UnknownOutcome,
        error => error,
    }
}

async fn move_message(
    connection: &mut Connection,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
    destination: &str,
) -> Result<Option<(u32, u32)>> {
    if mailbox == destination {
        return Err(Error::Invalid("source and destination must differ".into()));
    }
    if !connection.move_supported && !connection.uidplus {
        return Err(Error::Unsupported(
            "server requires MOVE or UIDPLUS for safe moves".into(),
        ));
    }
    verify_message(connection, mailbox, uidvalidity, uid).await?;
    let mapping = mapped_copy_or_move(
        &mut connection.session,
        uid,
        destination,
        connection.move_supported,
    )
    .await?;
    if !connection.move_supported {
        let mut stream = connection
            .session
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .await
            .map_err(|_| Error::UnknownOutcome)?;
        while stream
            .try_next()
            .await
            .map_err(|_| Error::UnknownOutcome)?
            .is_some()
        {}
        drop(stream);
        let stream = connection
            .session
            .uid_expunge(uid.to_string())
            .await
            .map_err(|_| Error::UnknownOutcome)?;
        futures_util::pin_mut!(stream);
        while stream
            .try_next()
            .await
            .map_err(|_| Error::UnknownOutcome)?
            .is_some()
        {}
    }
    Ok(mapping)
}

async fn mapped_copy_or_move<
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + std::fmt::Debug,
>(
    session: &mut Session<T>,
    uid: u32,
    destination: &str,
    moving: bool,
) -> Result<Option<(u32, u32)>> {
    use async_imap::imap_proto::{Response, ResponseCode, Status, UidSetMember};
    if destination.is_empty() || destination.chars().any(char::is_control) {
        return Err(Error::Invalid("invalid destination folder".into()));
    }
    let destination = destination.replace('\\', "\\\\").replace('"', "\\\"");
    let command = format!(
        "UID {} {uid} \"{destination}\"",
        if moving { "MOVE" } else { "COPY" }
    );
    let tag = session.run_command(command).await.map_err(mutation_error)?;
    let mut mapping = None;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|_| Error::UnknownOutcome)?
            .ok_or(Error::UnknownOutcome)?;
        let code = match response.parsed() {
            Response::Done { code, .. } | Response::Data { code, .. } => code.as_ref(),
            _ => None,
        };
        if let Some(ResponseCode::CopyUid(validity, source, destination)) = code {
            let single = |set: &[UidSetMember]| -> Option<u32> {
                match set {
                    [UidSetMember::Uid(value)] => Some(*value),
                    [UidSetMember::UidRange(range)] if range.start() == range.end() => {
                        Some(*range.start())
                    }
                    _ => None,
                }
            };
            if single(source) == Some(uid) {
                mapping = single(destination).map(|destination| (*validity, destination));
            }
        }
        if let Response::Done {
            tag: response_tag,
            status,
            ..
        } = response.parsed()
        {
            if *response_tag != tag {
                return Err(Error::UnknownOutcome);
            }
            return match status {
                Status::Ok => Ok(mapping),
                Status::No | Status::Bad => Err(Error::Invalid("mail server rejected move".into())),
                _ => Err(Error::UnknownOutcome),
            };
        }
        if matches!(
            response.parsed(),
            Response::Data {
                status: Status::Bye,
                ..
            }
        ) {
            return Err(Error::UnknownOutcome);
        }
    }
}

type StoredMove = (
    Option<String>,
    Value,
    Value,
    Option<chrono::NaiveDateTime>,
    u32,
);

#[allow(clippy::too_many_arguments)]
async fn persist_move(
    db: &Db,
    account: &Account,
    source: &str,
    validity: u32,
    uid: u32,
    destination: &str,
    mapping: Option<(u32, u32)>,
) -> Result<()> {
    let mut tx = db.pool.begin().await?;
    let row: Option<StoredMove> = sqlx::query_as("SELECT rustbox_id,flags,metadata,internal_date,size FROM messages WHERE account_id=? AND mailbox=? AND uidvalidity=? AND uid=? FOR UPDATE")
        .bind(&account.id).bind(source).bind(validity).bind(uid).fetch_optional(&mut *tx).await?;
    if let Some((Some(id), flags, metadata, date, size)) = row {
        sqlx::query("UPDATE message_locations SET active=FALSE WHERE message_id=? AND folder=?")
            .bind(&id)
            .bind(source)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM messages WHERE account_id=? AND mailbox=? AND uidvalidity=? AND uid=?",
        )
        .bind(&account.id)
        .bind(source)
        .bind(validity)
        .bind(uid)
        .execute(&mut *tx)
        .await?;
        if let Some((next_validity, next_uid)) = mapping {
            sqlx::query("INSERT INTO message_locations(message_id,folder,uidvalidity,uid) VALUES(?,?,?,?) ON DUPLICATE KEY UPDATE uidvalidity=VALUES(uidvalidity),uid=VALUES(uid),active=TRUE").bind(&id).bind(destination).bind(next_validity).bind(next_uid).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO messages(account_id,mailbox,uidvalidity,uid,flags,metadata,internal_date,size,rustbox_id) VALUES(?,?,?,?,?,?,?,?,?)").bind(&account.id).bind(destination).bind(next_validity).bind(next_uid).bind(flags).bind(metadata).bind(date).bind(size).bind(&id).execute(&mut *tx).await?;
            sqlx::query("UPDATE mail_messages SET identity_confidence='confirmed_move',removed_at=NULL WHERE id=?").bind(&id).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE mail_messages SET identity_confidence='unresolved_move',removed_at=UTC_TIMESTAMP(6) WHERE id=?").bind(&id).execute(&mut *tx).await?;
        }
        db.emit(&mut tx,&account.id,"message.moved",json!({"message_id":id,"source":source,"destination":destination,"destination_uidvalidity":mapping.map(|v|v.0),"destination_uid":mapping.map(|v|v.1),"identity_confirmed":mapping.is_some()})).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn reconcile(
    connection: &mut Connection,
    db: &Db,
    account: &Account,
    include_folders: bool,
    full: bool,
) -> Result<()> {
    let mut folders = vec!["INBOX".to_owned()];
    if include_folders {
        folders.extend(
            account
                .config
                .watched_folders
                .iter()
                .filter(|f| !f.eq_ignore_ascii_case("INBOX"))
                .cloned(),
        );
    }
    for folder in folders {
        if folder != "INBOX" && !full {
            let fields = if connection.condstore {
                "(MESSAGES UIDNEXT UIDVALIDITY UNSEEN HIGHESTMODSEQ)"
            } else {
                "(MESSAGES UIDNEXT UIDVALIDITY UNSEEN)"
            };
            let status = connection
                .session
                .status(&folder, fields)
                .await
                .map_err(protocol)?;
            let fingerprint = format!("{status:?}");
            if connection.status.get(&folder) == Some(&fingerprint) {
                continue;
            }
            sync_folder(connection, db, account, &folder, full).await?;
            connection.status.insert(folder, fingerprint);
        } else {
            sync_folder(connection, db, account, &folder, full).await?;
        }
    }
    connection.session.select("INBOX").await.map_err(protocol)?;
    while connection.session.unsolicited_responses.try_recv().is_ok() {}
    Ok(())
}

async fn sync_folder(
    connection: &mut Connection,
    db: &Db,
    account: &Account,
    folder: &str,
    full: bool,
) -> Result<()> {
    let selected = if connection.condstore {
        connection.session.select_condstore(folder).await
    } else {
        connection.session.select(folder).await
    }
    .map_err(protocol)?;
    let validity = selected
        .uid_validity
        .ok_or_else(|| Error::Unsupported("persistent UIDs are required".into()))?;
    let ceiling = selected.uid_next.unwrap_or(1).saturating_sub(1);
    let row = sqlx::query_as::<_, Checkpoint>("SELECT uidvalidity,last_uid,highest_modseq,initial_uid_boundary,backfill_complete FROM mailbox_checkpoints WHERE account_id=? AND mailbox=?").bind(&account.id).bind(folder).fetch_optional(&db.pool).await?;
    let reset = row.as_ref().is_some_and(|r| r.uidvalidity != validity);
    if row.is_none() || reset {
        let mut tx = db.pool.begin().await?;
        sqlx::query("UPDATE message_locations l JOIN mail_messages m ON m.id=l.message_id SET l.active=FALSE WHERE m.account_id=? AND l.folder=?").bind(&account.id).bind(folder).execute(&mut *tx).await?;
        sqlx::query("UPDATE mail_messages m SET removed_at=UTC_TIMESTAMP(6) WHERE m.account_id=? AND NOT EXISTS (SELECT 1 FROM message_locations l WHERE l.message_id=m.id AND l.active=TRUE)").bind(&account.id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM messages WHERE account_id=? AND mailbox=?")
            .bind(&account.id)
            .bind(folder)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mailbox_checkpoints(account_id,mailbox,uidvalidity,last_uid,highest_modseq,initial_uid_boundary,backfill_complete) VALUES(?,?,?,0,NULL,?,FALSE) ON DUPLICATE KEY UPDATE uidvalidity=VALUES(uidvalidity),last_uid=0,highest_modseq=NULL,initial_uid_boundary=VALUES(initial_uid_boundary),backfill_complete=FALSE").bind(&account.id).bind(folder).bind(validity).bind(ceiling).execute(&mut *tx).await?;
        if reset {
            db.emit(
                &mut tx,
                &account.id,
                "account.sync_reset",
                json!({"mailbox":folder,"uidvalidity":validity}),
            )
            .await?;
        }
        tx.commit().await?;
    }
    let initial = reset || row.as_ref().is_none_or(|r| !r.backfill_complete);
    let last_uid = if reset {
        0
    } else {
        row.as_ref().map(|r| r.last_uid).unwrap_or(0)
    };
    let boundary = if reset {
        ceiling
    } else {
        row.as_ref()
            .map(|r| r.initial_uid_boundary)
            .unwrap_or(ceiling)
    };
    let modseq = if reset {
        None
    } else {
        row.as_ref().and_then(|r| r.highest_modseq)
    };
    let query = if initial {
        format!(
            "SINCE {}",
            (account
                .config
                .sync_from
                .ok_or_else(|| Error::Internal("persisted sync cutoff missing".into()))?
                - chrono::Duration::days(1))
            .format("%d-%b-%Y")
        )
    } else {
        format!("UID {}:*", last_uid.saturating_add(1))
    };
    let mut new_uids: Vec<u32> = connection
        .session
        .uid_search(query)
        .await
        .map_err(protocol)?
        .into_iter()
        .filter(|uid| initial || *uid > last_uid)
        .collect();
    if initial {
        new_uids.extend(
            connection
                .session
                .uid_search(format!("UID {}:*", boundary.saturating_add(1)))
                .await
                .map_err(protocol)?
                .into_iter()
                .filter(|uid| *uid > boundary),
        );
    }
    new_uids.sort_unstable();
    new_uids.dedup();
    new_uids.sort_by_key(|uid| (*uid <= boundary, *uid));
    for chunk in new_uids.chunks(100) {
        fetch_metadata(
            connection,
            db,
            account,
            folder,
            (
                validity,
                boundary,
                if reset {
                    "recovery"
                } else if initial {
                    "initial_sync"
                } else {
                    "live"
                },
            ),
            &uid_set(chunk),
            None,
        )
        .await?;
    }
    if !initial
        && connection.condstore
        && !full
        && let Some(modseq) = modseq
    {
        fetch_metadata(
            connection,
            db,
            account,
            folder,
            (
                validity,
                boundary,
                if reset {
                    "recovery"
                } else if initial {
                    "initial_sync"
                } else {
                    "live"
                },
            ),
            "1:*",
            Some(modseq),
        )
        .await?;
    }
    let check_index = full
        || connection
            .next_index_check
            .get(folder)
            .is_none_or(|at| Instant::now() >= *at);
    if !initial && check_index {
        let known = sqlx::query_scalar::<_,u32>("SELECT uid FROM messages WHERE account_id=? AND mailbox=? AND uidvalidity=? ORDER BY uid").bind(&account.id).bind(folder).bind(validity).fetch_all(&db.pool).await?;
        for chunk in known.chunks(200) {
            let ids = uid_set(chunk);
            let present = connection
                .session
                .uid_search(format!("UID {ids}"))
                .await
                .map_err(protocol)?;
            let gone: Vec<u32> = chunk
                .iter()
                .filter(|uid| !present.contains(uid))
                .copied()
                .collect();
            if !gone.is_empty() {
                let mut tx = db.pool.begin().await?;
                for uid in gone {
                    let message_id: Option<String> = sqlx::query_scalar("SELECT rustbox_id FROM messages WHERE account_id=? AND mailbox=? AND uidvalidity=? AND uid=?")
                        .bind(&account.id).bind(folder).bind(validity).bind(uid).fetch_optional(&mut *tx).await?.flatten();
                    if let Some(id) = &message_id {
                        sqlx::query("UPDATE message_locations SET active=FALSE WHERE message_id=? AND folder=?").bind(id).bind(folder).execute(&mut *tx).await?;
                        sqlx::query("UPDATE mail_messages SET removed_at=UTC_TIMESTAMP(6) WHERE id=? AND NOT EXISTS (SELECT 1 FROM message_locations WHERE message_id=? AND active=TRUE)").bind(id).bind(id).execute(&mut *tx).await?;
                    }
                    sqlx::query("DELETE FROM messages WHERE account_id=? AND mailbox=? AND uidvalidity=? AND uid=?").bind(&account.id).bind(folder).bind(validity).bind(uid).execute(&mut *tx).await?;
                    db.emit(
                        &mut tx,
                        &account.id,
                        "message.removed",
                        json!({"message_id":message_id,"mailbox":folder,"uidvalidity":validity,"uid":uid}),
                    )
                    .await?;
                }
                tx.commit().await?;
            }
            if full {
                let mut present: Vec<u32> = present.into_iter().collect();
                present.sort_unstable();
                if !present.is_empty() {
                    fetch_metadata(
                        connection,
                        db,
                        account,
                        folder,
                        (
                            validity,
                            boundary,
                            if reset {
                                "recovery"
                            } else if initial {
                                "initial_sync"
                            } else {
                                "live"
                            },
                        ),
                        &uid_set(&present),
                        None,
                    )
                    .await?;
                }
            }
        }
    }
    if check_index || initial {
        connection
            .next_index_check
            .insert(folder.to_owned(), Instant::now() + Duration::from_secs(120));
    }
    let mut tx = db.pool.begin().await?;
    sqlx::query("UPDATE mailbox_checkpoints SET last_uid=GREATEST(last_uid,?),highest_modseq=?,backfill_complete=TRUE,updated_at=UTC_TIMESTAMP(6) WHERE account_id=? AND mailbox=?").bind(ceiling).bind(selected.highest_modseq).bind(&account.id).bind(folder).execute(&mut *tx).await?;
    if initial {
        db.emit(
            &mut tx,
            &account.id,
            "account.sync_completed",
            json!({"mailbox":folder,"uidvalidity":validity,"sync_from":account.config.sync_from}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn flag_name(flag: async_imap::types::Flag<'_>) -> Option<String> {
    use async_imap::types::Flag;
    Some(match flag {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Recent => return None,
        Flag::MayCreate => "\\*".into(),
        Flag::Custom(value) => value.into_owned(),
    })
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

async fn fetch_metadata(
    connection: &mut Connection,
    db: &Db,
    account: &Account,
    folder: &str,
    epoch: (u32, u32, &str),
    uids: &str,
    modseq: Option<u64>,
) -> Result<()> {
    let suffix = modseq
        .map(|m| format!(" (CHANGEDSINCE {m})"))
        .unwrap_or_default();
    let query = format!("(UID FLAGS RFC822.SIZE INTERNALDATE ENVELOPE BODY.PEEK[HEADER]){suffix}");
    let mut stream = connection
        .session
        .uid_fetch(uids, query)
        .await
        .map_err(protocol)?;
    let mut messages = Vec::new();
    while let Some(fetch) = stream.try_next().await.map_err(protocol)? {
        let Some(uid) = fetch.uid else {
            continue;
        };
        let mut flags: Vec<String> = fetch.flags().filter_map(flag_name).collect();
        flags.sort();
        let text = |bytes: Option<&[u8]>| bytes.map(|s| String::from_utf8_lossy(s).into_owned());
        let envelope = fetch.envelope();
        let envelope_metadata = json!({"subject":text(envelope.and_then(|e| e.subject.as_deref())),"message_id":text(envelope.and_then(|e| e.message_id.as_deref())),"date":text(envelope.and_then(|e| e.date.as_deref())),"from":envelope.map(|e| e.from.iter().flatten().map(|a| json!({"name":text(a.name.as_deref()),"mailbox":text(a.mailbox.as_deref()),"host":text(a.host.as_deref())})).collect::<Vec<_>>()),"to":envelope.map(|e| e.to.iter().flatten().map(|a| json!({"name":text(a.name.as_deref()),"mailbox":text(a.mailbox.as_deref()),"host":text(a.host.as_deref())})).collect::<Vec<_>>())});
        let metadata = match fetch.header() {
            Some(header) => crate::mail::metadata(header)?,
            None => envelope_metadata,
        };
        messages.push((
            uid,
            flags,
            metadata,
            fetch.internal_date().map(|d| d.with_timezone(&Utc)),
            fetch.size.unwrap_or(0),
        ));
        if modseq.is_some() && messages.len() >= 100 {
            persist_metadata(
                db,
                account,
                folder,
                epoch,
                std::mem::take(&mut messages),
                true,
            )
            .await?;
        }
    }
    drop(stream);
    persist_metadata(db, account, folder, epoch, messages, modseq.is_some()).await
}

type MessageMetadata = (u32, Vec<String>, Value, Option<DateTime<Utc>>, u32);

async fn persist_metadata(
    db: &Db,
    account: &Account,
    folder: &str,
    epoch: (u32, u32, &str),
    messages: Vec<MessageMetadata>,
    changed_only: bool,
) -> Result<()> {
    let (validity, _boundary, discovery_reason) = epoch;
    let mut tx = db.pool.begin().await?;
    let maximum = messages.iter().map(|m| m.0).max();
    for (uid, flags, metadata, date, size) in messages {
        let Some(received) = date else {
            return Err(Error::Unavailable(
                "IMAP metadata is missing INTERNALDATE".into(),
            ));
        };
        if account
            .config
            .sync_from
            .is_some_and(|cutoff| received < cutoff)
        {
            continue;
        }
        let existing = sqlx::query_as::<_,(Value,Option<String>)>("SELECT flags,rustbox_id FROM messages WHERE account_id=? AND mailbox=? AND uidvalidity=? AND uid=?")
            .bind(&account.id).bind(folder).bind(validity).bind(uid).fetch_optional(&mut *tx).await?;
        let rustbox_id = existing
            .as_ref()
            .and_then(|(_, id)| id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let value = json!(flags);
        let confidence = if discovery_reason == "recovery" {
            "possible_rediscovery"
        } else {
            "location"
        };
        sqlx::query("INSERT INTO mail_messages (id,account_id,backend,metadata,flags,received_at,size,discovery_reason,identity_confidence) VALUES (?,?,'imap',?,?,?,?,?,?) ON DUPLICATE KEY UPDATE metadata=VALUES(metadata),flags=VALUES(flags),received_at=VALUES(received_at),size=VALUES(size),removed_at=NULL,updated_at=UTC_TIMESTAMP(6)")
            .bind(&rustbox_id).bind(&account.id).bind(&metadata).bind(&value).bind(received.naive_utc()).bind(u64::from(size)).bind(discovery_reason).bind(confidence).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO message_locations (message_id,folder,uidvalidity,uid) VALUES (?,?,?,?) ON DUPLICATE KEY UPDATE uidvalidity=VALUES(uidvalidity),uid=VALUES(uid),active=TRUE")
            .bind(&rustbox_id).bind(folder).bind(validity).bind(uid).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO messages(account_id,mailbox,uidvalidity,uid,flags,metadata,internal_date,size,rustbox_id) VALUES(?,?,?,?,?,?,?,?,?) ON DUPLICATE KEY UPDATE flags=VALUES(flags),metadata=VALUES(metadata),internal_date=VALUES(internal_date),size=VALUES(size),rustbox_id=VALUES(rustbox_id)")
            .bind(&account.id).bind(folder).bind(validity).bind(uid).bind(&value).bind(&metadata).bind(received.naive_utc()).bind(size).bind(&rustbox_id).execute(&mut *tx).await?;
        let event = match existing {
            None => Some("message.created"),
            Some((previous, _)) if previous != value => Some("message.flags_changed"),
            _ => None,
        };
        if let Some(event) = event {
            db.emit(&mut tx, &account.id, event, json!({"message_id":rustbox_id,"mailbox":folder,"uidvalidity":validity,"uid":uid,"flags":flags,"metadata":metadata,"content_state":"pending","discovery_reason":discovery_reason,"identity_confidence":confidence})).await?;
        }
    }
    if !changed_only && let Some(maximum) = maximum {
        sqlx::query("UPDATE mailbox_checkpoints SET last_uid=GREATEST(last_uid,?) WHERE account_id=? AND mailbox=?").bind(maximum).bind(&account.id).bind(folder).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::extensions::idle::IdleResponse;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn idle_interrupt_completes_done_and_reuses_session() {
        let (client, server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            server
                .get_mut()
                .write_all(b"* OK mock ready\r\n")
                .await
                .unwrap();
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let tag = line.split_whitespace().next().unwrap().to_owned();
            assert!(line.contains(" LOGIN "));
            server
                .get_mut()
                .write_all(format!("{tag} OK logged in\r\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            let tag = line.split_whitespace().next().unwrap().to_owned();
            assert!(line.contains(" IDLE"));
            server.get_mut().write_all(b"+ idling\r\n").await.unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            assert_eq!(line, "DONE\r\n");
            server
                .get_mut()
                .write_all(format!("{tag} OK idle done\r\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            assert!(line.contains(" NOOP"));
            let tag = line.split_whitespace().next().unwrap();
            server
                .get_mut()
                .write_all(format!("{tag} OK noop\r\n").as_bytes())
                .await
                .unwrap();
        });
        let mut client = Client::new(client);
        client.read_response().await.unwrap();
        let session = client.login("user", "password").await.unwrap();
        let mut idle = session.idle();
        idle.init().await.unwrap();
        {
            let (wait, interrupt) = idle.wait();
            drop(interrupt);
            assert_eq!(wait.await.unwrap(), IdleResponse::ManualInterrupt);
        }
        let mut session = tokio::time::timeout(Duration::from_secs(1), idle.done())
            .await
            .unwrap()
            .unwrap();
        session.noop().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idle_disconnect_cannot_be_reused() {
        let (client, server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            server
                .get_mut()
                .write_all(b"* OK mock ready\r\n")
                .await
                .unwrap();
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let tag = line.split_whitespace().next().unwrap();
            server
                .get_mut()
                .write_all(format!("{tag} OK logged in\r\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            server.get_mut().write_all(b"+ idling\r\n").await.unwrap();
        });
        let mut client = Client::new(client);
        client.read_response().await.unwrap();
        let session = client.login("user", "password").await.unwrap();
        let mut idle = session.idle();
        idle.init().await.unwrap();
        {
            let (wait, _interrupt) = idle.wait();
            let _ = tokio::time::timeout(Duration::from_secs(1), wait).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(1), idle.done())
                .await
                .unwrap()
                .is_err()
        );
        server.await.unwrap();
    }
    async fn scripted_session(
        script: Vec<(&'static str, &'static str)>,
    ) -> (
        Session<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<()>,
    ) {
        let (client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            server.get_mut().write_all(b"* OK ready\r\n").await.unwrap();
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let tag = line.split_whitespace().next().unwrap();
            server
                .get_mut()
                .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                .await
                .unwrap();
            for (expected, response) in script {
                line.clear();
                server.read_line(&mut line).await.unwrap();
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                assert_eq!(command, expected);
                server
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
                server
                    .get_mut()
                    .write_all(format!("{tag} OK done\r\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let mut client = Client::new(client);
        client.read_response().await.unwrap();
        (client.login("user", "password").await.unwrap(), task)
    }

    #[tokio::test]
    async fn peek_partial_fetch_consumes_literal_and_tagged_completion() {
        let (mut session, server) = scripted_session(vec![
            (
                "UID FETCH 42 (UID BODY.PEEK[]<0.65536>)",
                "* 1 FETCH (UID 42 BODY[]<0> {5}\r\nhello)\r\n",
            ),
            ("NOOP", ""),
        ])
        .await;
        {
            let mut stream = session
                .uid_fetch("42", "(UID BODY.PEEK[]<0.65536>)")
                .await
                .unwrap();
            let fetch = stream.try_next().await.unwrap().unwrap();
            assert_eq!(fetch.body(), Some(&b"hello"[..]));
            assert!(stream.try_next().await.unwrap().is_none());
        }
        session.noop().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn targeted_move_and_uid_expunge_preserve_session() {
        let (mut session, server) = scripted_session(vec![
            ("UID MOVE 42 \"Trash\"", "* 1 EXPUNGE\r\n"),
            ("UID EXPUNGE 43", "* 2 EXPUNGE\r\n"),
            ("NOOP", ""),
        ])
        .await;
        session.uid_mv("42", "Trash").await.unwrap();
        {
            let stream = session.uid_expunge("43").await.unwrap();
            futures_util::pin_mut!(stream);
            while stream.try_next().await.unwrap().is_some() {}
        }
        session.noop().await.unwrap();
        server.await.unwrap();
    }
    #[tokio::test]
    async fn xoauth2_challenge_failure_is_completed_without_repeating_token() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            server.get_mut().write_all(b"* OK ready\r\n").await.unwrap();
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            let tag = tag.to_owned();
            assert_eq!(command, "AUTHENTICATE XOAUTH2");
            server.get_mut().write_all(b"+ \r\n").await.unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            assert_eq!(
                STANDARD.decode(line.trim()).unwrap(),
                b"user=person@example.com\x01auth=Bearer test-token\x01\x01"
            );
            server
                .get_mut()
                .write_all(b"+ eyJzdGF0dXMiOiI0MDEifQ==\r\n")
                .await
                .unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            assert_eq!(line, "\r\n");
            server
                .get_mut()
                .write_all(format!("{tag} NO authentication failed\r\n").as_bytes())
                .await
                .unwrap();
        });
        let mut client = Client::new(client);
        client.read_response().await.unwrap();
        let result = client
            .authenticate("XOAUTH2", Xoauth2::new("person@example.com", "test-token"))
            .await;
        assert!(matches!(result, Err((async_imap::error::Error::No(_), _))));
        task.await.unwrap();
    }
    #[tokio::test]
    async fn move_copyuid_mapping_is_confirmed_only_for_exact_source() {
        let (mut session, peer) = scripted_session(vec![
            ("UID MOVE 42 \"Trash\"", "* OK [COPYUID 9 42 101] moved\r\n"),
            ("NOOP", ""),
        ])
        .await;
        assert_eq!(
            mapped_copy_or_move(&mut session, 42, "Trash", true)
                .await
                .unwrap(),
            Some((9, 101))
        );
        session.noop().await.unwrap();
        peer.await.unwrap();
        let (mut session, peer) = scripted_session(vec![(
            "UID MOVE 42 \"Trash\"",
            "* OK [COPYUID 9 43 101] mismatch\r\n",
        )])
        .await;
        assert_eq!(
            mapped_copy_or_move(&mut session, 42, "Trash", true)
                .await
                .unwrap(),
            None
        );
        peer.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn persisted_cutoff_recovers_initial_and_reset_messages() -> anyhow::Result<()> {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&std::env::var("TEST_DATABASE_URL")?)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Db::new(pool, crate::crypto::Crypto::new(&[42; 32])?);
        let cutoff = Utc::now() - chrono::Duration::days(2);
        let input = serde_json::from_value(json!({"name":"cutoff test","sync_from":cutoff,
            "imap":{"host":"mail.example.com","port":993,"username":"test","password":"secret"},
            "smtp":{"host":"mail.example.com","port":465,"username":"test","password":"secret"}}))?;
        let account = db.create_account(input).await?;
        let metadata = json!({"message_id":"<cutoff@test>"});
        persist_metadata(
            &db,
            &account,
            "INBOX",
            (1, 100, "initial_sync"),
            vec![
                (
                    1,
                    vec![],
                    metadata.clone(),
                    Some(cutoff - chrono::Duration::seconds(1)),
                    100,
                ),
                (2, vec![], metadata.clone(), Some(cutoff), 100),
            ],
            false,
        )
        .await?;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM mail_messages WHERE account_id=?")
                .bind(&account.id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(count, 1);
        persist_metadata(
            &db,
            &account,
            "INBOX",
            (1, 100, "initial_sync"),
            vec![(2, vec![], metadata.clone(), Some(cutoff), 100)],
            false,
        )
        .await?;
        persist_metadata(
            &db,
            &account,
            "INBOX",
            (2, 100, "recovery"),
            vec![(2, vec![], metadata, Some(cutoff), 100)],
            false,
        )
        .await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE account_id=? AND event_type='message.created'",
        )
        .bind(&account.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(count, 2);
        let confidence:String=sqlx::query_scalar("SELECT identity_confidence FROM mail_messages WHERE account_id=? ORDER BY created_at DESC LIMIT 1").bind(&account.id).fetch_one(&db.pool).await?;
        assert_eq!(confidence, "possible_rediscovery");
        db.delete_account(&account.id).await?;
        Ok(())
    }
}
