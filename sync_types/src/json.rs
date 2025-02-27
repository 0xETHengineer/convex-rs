use anyhow::bail;
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
};
use serde_json::{
    json,
    Value as JsonValue,
};

use crate::{
    types::ClientEvent,
    AuthenticationToken,
    ClientMessage,
    IdentityVersion,
    LogLines,
    Query,
    QueryFailure,
    QueryId,
    QuerySetModification,
    SerializedQueryJournal,
    ServerMessage,
    SessionRequestSeqNumber,
    StateModification,
    StateVersion,
    Timestamp,
    UserIdentifier,
    UserIdentityAttributes,
};

/// We implement custom deserialize and serialize to deliver u64s to
/// JavaScript. JavaScript's number type can only fit 52 bits of precision, so
/// u64s larger than 2^53-1 need to get stuffed in a BigInt instead. Sending
/// down a number in JSON would cause it to get decoded into a number
/// by default, with loss of precision.
///
/// e.g. (this number is 2^60)
///   > JSON.parse("{\"foo\": 1152921504606846976}")
///   { foo: 1152921504606847000 }
///
/// So instead we send it down as a string and unpack it ourselves.
fn u64_to_string(x: u64) -> String {
    base64::encode(x.to_le_bytes())
}

fn string_to_u64(s: &str) -> anyhow::Result<u64> {
    let bytes: [u8; 8] = base64::decode(s)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("u64 must be 8 bytes"))?;
    Ok(u64::from_le_bytes(bytes))
}

/// A custom deserializer for optional fields.
/// The outer `Option` represents the field being missing and the inner
/// `Option` represents null.
pub fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}
#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
struct QueryJson {
    query_id: QueryId,
    udf_path: String,
    args: JsonValue,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "double_option")]
    journal: Option<SerializedQueryJournal>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type")]
enum QuerySetModificationJson {
    Add(QueryJson),
    #[serde(rename_all = "camelCase")]
    Remove {
        query_id: QueryId,
    },
}

impl TryFrom<QuerySetModification> for JsonValue {
    type Error = anyhow::Error;

    fn try_from(m: QuerySetModification) -> Result<Self, Self::Error> {
        let modification_json = match m {
            QuerySetModification::Add(q) => {
                let query_json = QueryJson {
                    query_id: q.query_id,
                    udf_path: String::from(q.udf_path),
                    args: JsonValue::from(q.args),
                    journal: q.journal,
                };
                QuerySetModificationJson::Add(query_json)
            },
            QuerySetModification::Remove { query_id } => {
                QuerySetModificationJson::Remove { query_id }
            },
        };
        Ok(serde_json::to_value(modification_json)?)
    }
}

impl TryFrom<JsonValue> for QuerySetModification {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        let m: QuerySetModificationJson = serde_json::from_value(value)?;
        let result = match m {
            QuerySetModificationJson::Add(q) => {
                let args: Vec<JsonValue> = serde_json::from_value(q.args)?;

                let query = Query {
                    query_id: q.query_id,
                    udf_path: q.udf_path.parse()?,
                    args,
                    journal: q.journal,
                };
                QuerySetModification::Add(query)
            },
            QuerySetModificationJson::Remove { query_id } => {
                QuerySetModification::Remove { query_id }
            },
        };
        Ok(result)
    }
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(tag = "tokenType")]
enum AuthenticationTokenJson {
    Admin {
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(alias = "impersonating")]
        acting_as: Option<JsonValue>,
    },
    User {
        value: String,
    },
    None,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(tag = "type")]
enum ClientMessageJson {
    #[serde(rename_all = "camelCase")]
    Connect {
        session_id: String,
        connection_count: u32,

        #[serde(default)]
        #[serde(skip_serializing_if = "Option::is_none")]
        last_close_reason: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ModifyQuerySet {
        base_version: u32,
        new_version: u32,
        modifications: Vec<JsonValue>,
    },
    #[serde(rename_all = "camelCase")]
    Mutation {
        // TODO(presley): Delete mutation_id and make request_id non optional
        // when we deprecate convex 0.6.0
        mutation_id: Option<u32>,
        request_id: Option<u32>,
        udf_path: String,
        args: JsonValue,
    },
    #[serde(rename_all = "camelCase")]
    Action {
        // TODO(presley): Delete action_id and make request_id non optional
        // when we deprecate convex 0.6.0
        action_id: Option<u32>,
        request_id: Option<u32>,
        udf_path: String,
        args: JsonValue,
    },
    #[serde(rename_all = "camelCase")]
    Authenticate {
        base_version: u32,
        #[serde(flatten)]
        token: AuthenticationTokenJson,
    },
    #[serde(rename_all = "camelCase")]
    Event {
        event_type: String,
        event: JsonValue,
    },
}

impl TryFrom<ClientMessage> for JsonValue {
    type Error = anyhow::Error;

    fn try_from(m: ClientMessage) -> Result<Self, Self::Error> {
        let s: ClientMessageJson = match m {
            ClientMessage::Connect {
                session_id,
                connection_count,
                last_close_reason,
            } => ClientMessageJson::Connect {
                session_id: format!("{}", session_id.as_hyphenated()),
                connection_count,
                last_close_reason: Some(last_close_reason),
            },
            ClientMessage::ModifyQuerySet {
                base_version,
                new_version,
                modifications,
            } => ClientMessageJson::ModifyQuerySet {
                base_version,
                new_version,
                modifications: modifications
                    .into_iter()
                    .map(JsonValue::try_from)
                    .collect::<anyhow::Result<Vec<_>>>()?,
            },
            ClientMessage::Mutation {
                request_id,
                udf_path,
                args,
            } => ClientMessageJson::Mutation {
                request_id: Some(request_id),
                mutation_id: Some(request_id),
                udf_path: String::from(udf_path),
                args: JsonValue::Array(args.into_iter().map(JsonValue::from).collect::<Vec<_>>()),
            },
            ClientMessage::Action {
                request_id,
                udf_path,
                args,
            } => ClientMessageJson::Action {
                request_id: Some(request_id),
                action_id: Some(request_id),
                udf_path: String::from(udf_path),
                args: JsonValue::Array(args.into_iter().map(JsonValue::from).collect::<Vec<_>>()),
            },
            ClientMessage::Authenticate {
                base_version,
                token: AuthenticationToken::Admin(value, acting_as),
            } => ClientMessageJson::Authenticate {
                base_version,
                token: AuthenticationTokenJson::Admin {
                    value,
                    acting_as: acting_as.map(|a| a.try_into()).transpose()?,
                },
            },
            ClientMessage::Authenticate {
                base_version,
                token: AuthenticationToken::User(value),
            } => ClientMessageJson::Authenticate {
                base_version,
                token: AuthenticationTokenJson::User { value },
            },
            ClientMessage::Authenticate {
                base_version,
                token: AuthenticationToken::None,
            } => ClientMessageJson::Authenticate {
                base_version,
                token: AuthenticationTokenJson::None,
            },
            ClientMessage::Event(ClientEvent { event_type, event }) => {
                ClientMessageJson::Event { event_type, event }
            },
        };
        let result = serde_json::to_value(s)?;
        Ok(result)
    }
}

impl TryFrom<JsonValue> for ClientMessage {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        let m: ClientMessageJson = serde_json::from_value(value)?;
        let result = match m {
            ClientMessageJson::Connect {
                session_id,
                connection_count,
                last_close_reason,
            } => ClientMessage::Connect {
                session_id: session_id.parse()?,
                connection_count,
                last_close_reason: last_close_reason.unwrap_or_else(|| "unknown".to_string()),
            },
            ClientMessageJson::ModifyQuerySet {
                base_version,
                new_version,
                modifications,
            } => ClientMessage::ModifyQuerySet {
                base_version,
                new_version,
                modifications: modifications
                    .into_iter()
                    .map(QuerySetModification::try_from)
                    .collect::<anyhow::Result<_>>()?,
            },
            ClientMessageJson::Mutation {
                request_id,
                mutation_id,
                udf_path,
                args,
            } => {
                let json_args: Vec<JsonValue> = serde_json::from_value(args)?;

                let request_id = if let Some(request_id) = request_id {
                    request_id
                } else {
                    mutation_id.ok_or_else(|| {
                        anyhow::anyhow!("Either mutation_id or request_id must be set")
                    })?
                };
                ClientMessage::Mutation {
                    request_id,
                    udf_path: udf_path.parse()?,
                    args: json_args,
                }
            },
            ClientMessageJson::Action {
                request_id,
                action_id,
                udf_path,
                args,
            } => {
                let json_args: Vec<JsonValue> = serde_json::from_value(args)?;

                let request_id = if let Some(request_id) = request_id {
                    request_id
                } else {
                    action_id.ok_or_else(|| {
                        anyhow::anyhow!("Either mutation_id or request_id must be set")
                    })?
                };
                ClientMessage::Action {
                    request_id,
                    udf_path: udf_path.parse()?,
                    args: json_args,
                }
            },
            ClientMessageJson::Authenticate {
                base_version,
                token,
            } => ClientMessage::Authenticate {
                base_version,
                token: match token {
                    AuthenticationTokenJson::Admin { value, acting_as } => {
                        AuthenticationToken::Admin(
                            value,
                            acting_as.map(TryInto::try_into).transpose()?,
                        )
                    },
                    AuthenticationTokenJson::User { value } => AuthenticationToken::User(value),
                    AuthenticationTokenJson::None => AuthenticationToken::None,
                },
            },
            ClientMessageJson::Event { event_type, event } => {
                ClientMessage::Event(ClientEvent { event_type, event })
            },
        };
        Ok(result)
    }
}

impl From<StateVersion> for JsonValue {
    fn from(v: StateVersion) -> Self {
        serde_json::json!({
            "querySet": v.query_set,
            "identity": v.identity,
            "ts": u64_to_string(v.ts.into()),
        })
    }
}

impl TryFrom<JsonValue> for StateVersion {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StateVersionJson {
            query_set: u32,
            identity: u32,
            ts: String,
        }
        let s: StateVersionJson = serde_json::from_value(value)?;
        Ok(Self {
            query_set: s.query_set,
            identity: s.identity,
            ts: Timestamp::try_from(string_to_u64(&s.ts)?)?,
        })
    }
}

impl<V: Into<JsonValue>> From<StateModification<V>> for JsonValue {
    fn from(m: StateModification<V>) -> Self {
        match m {
            StateModification::QueryUpdated {
                query_id,
                value,
                log_lines,
                journal,
            } => {
                let jv: JsonValue = value.into();
                json!({
                    "type": "QueryUpdated",
                    "queryId": query_id,
                    "value": jv,
                    "logLines": log_lines,
                    "journal": journal
                })
            },
            StateModification::QueryFailed {
                query_id,
                error_message,
                log_lines,
                journal,
            } => json!({
                "type": "QueryFailed",
                "queryId": query_id,
                "errorMessage": error_message,
                "logLines": log_lines,
                "journal": journal
            }),
            StateModification::QueryRemoved { query_id } => json!({
                "type": "QueryRemoved",
                "queryId": query_id,
            }),
        }
    }
}

impl<V: TryFrom<JsonValue, Error = anyhow::Error>> TryFrom<JsonValue> for StateModification<V> {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        #[allow(clippy::enum_variant_names)]
        #[derive(Deserialize)]
        #[serde(tag = "type")]
        pub enum StateModificationJson {
            #[serde(rename_all = "camelCase")]
            QueryUpdated {
                query_id: QueryId,
                value: JsonValue,
                log_lines: Vec<String>,
                journal: SerializedQueryJournal,
            },
            #[serde(rename_all = "camelCase")]
            QueryFailed {
                query_id: QueryId,
                error_message: String,
                log_lines: Vec<String>,
                journal: SerializedQueryJournal,
            },
            #[serde(rename_all = "camelCase")]
            QueryRemoved { query_id: QueryId },
        }
        let s: StateModificationJson = serde_json::from_value(value)?;
        let result = match s {
            StateModificationJson::QueryUpdated {
                query_id,
                value,
                log_lines,
                journal,
            } => StateModification::QueryUpdated {
                query_id,
                value: value.try_into()?,
                log_lines,
                journal,
            },
            StateModificationJson::QueryFailed {
                query_id,
                error_message,
                log_lines,
                journal,
            } => StateModification::QueryFailed {
                query_id,
                error_message,
                log_lines,
                journal,
            },
            StateModificationJson::QueryRemoved { query_id } => {
                StateModification::QueryRemoved { query_id }
            },
        };
        Ok(result)
    }
}

impl From<QueryFailure> for JsonValue {
    fn from(q: QueryFailure) -> Self {
        json!({
            "queryId": q.query_id,
            "message": q.message,
            "logLines": q.log_lines,
        })
    }
}

impl TryFrom<JsonValue> for QueryFailure {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct QueryFailureJson {
            query_id: u32,
            message: String,
            log_lines: Vec<String>,
        }
        let q: QueryFailureJson = serde_json::from_value(value)?;
        Ok(Self {
            query_id: QueryId::new(q.query_id),
            message: q.message,
            log_lines: q.log_lines,
        })
    }
}

impl<V: Into<JsonValue>> From<ServerMessage<V>> for JsonValue {
    fn from(m: ServerMessage<V>) -> Self {
        match m {
            ServerMessage::Transition {
                start_version,
                end_version,
                modifications,
            } => json!({
                "type": "Transition",
                "startVersion": JsonValue::from(start_version),
                "endVersion": JsonValue::from(end_version),
                "modifications": modifications.into_iter().map(JsonValue::from).collect::<Vec<JsonValue>>(),
            }),
            ServerMessage::QueriesFailed { failures } => json!({
                "type": "QueriesFailed",
                "failures": failures.into_iter().map(JsonValue::from).collect::<Vec<_>>(),
            }),
            ServerMessage::MutationResponse {
                request_id,
                result: Ok(value),
                ts,
                log_lines,
            } => {
                let jv: JsonValue = value.into();
                json!({
                    "type": "MutationResponse",
                    // TODO(presley): Delete when we deprecate convex 0.6.0.
                    "mutationId": request_id,
                    "requestId": request_id,
                    "success": true,
                    "result": jv,
                    "ts": ts.map(|ts| u64_to_string(ts.into())),
                    "logLines": log_lines,
                })
            },
            ServerMessage::MutationResponse {
                request_id,
                result: Err(s),
                ts,
                log_lines,
            } => json!({
                "type": "MutationResponse",
                // TODO(presley): Delete when we deprecate convex 0.6.0.
                "mutationId": request_id,
                "requestId": request_id,
                "success": false,
                "result": s,
                "ts": ts.map(|ts| u64_to_string(ts.into())),
                "logLines": log_lines,
            }),
            ServerMessage::ActionResponse {
                request_id,
                result: Ok(value),
                log_lines,
            } => {
                let jv: JsonValue = value.into();
                json!({
                    "type": "ActionResponse",
                    // TODO(presley): Delete when we deprecate convex 0.6.0.
                    "actionId": request_id,
                    "requestId": request_id,
                    "success": true,
                    "result": jv,
                    "logLines": log_lines,
                })
            },
            ServerMessage::ActionResponse {
                request_id,
                result: Err(s),
                log_lines,
            } => json!({
                "type": "ActionResponse",
                // TODO(presley): Delete when we deprecate convex 0.6.0.
                "actionId": request_id,
                "requestId": request_id,
                "success": false,
                "result": s,
                "logLines": log_lines,
            }),
            ServerMessage::AuthError {
                error_message,
                base_version,
            } => json!({
                "type": "AuthError",
                "error": error_message,
                "baseVersion": base_version,
            }),
            ServerMessage::FatalError { error_message } => json!({
                "type": "FatalError",
                "error": error_message,
            }),
            ServerMessage::Ping {} => json!({
                "type": "Ping"
            }),
        }
    }
}

impl<V: TryFrom<JsonValue, Error = anyhow::Error>> TryFrom<JsonValue> for ServerMessage<V> {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type")]
        pub enum ServerMessageJson {
            #[serde(rename_all = "camelCase")]
            Transition {
                start_version: JsonValue,
                end_version: JsonValue,
                modifications: Vec<JsonValue>,
            },
            #[serde(rename_all = "camelCase")]
            QueriesFailed { failures: Vec<JsonValue> },
            #[serde(rename_all = "camelCase")]
            MutationResponse {
                // TODO(presley): Delete mutation_id and make request_id non optional
                // when we deprecate old 0.6.0
                request_id: Option<SessionRequestSeqNumber>,
                mutation_id: Option<SessionRequestSeqNumber>,
                success: bool,
                result: JsonValue,
                ts: Option<String>,
                log_lines: LogLines,
            },
            #[serde(rename_all = "camelCase")]
            ActionResponse {
                // TODO(presley): Delete mutation_id and make request_id non optional
                // when we deprecate old 0.6.0
                request_id: Option<SessionRequestSeqNumber>,
                action_id: Option<SessionRequestSeqNumber>,
                success: bool,
                result: JsonValue,
                log_lines: LogLines,
            },
            #[serde(rename_all = "camelCase")]
            FatalError { error: String },
            #[serde(rename_all = "camelCase")]
            AuthError {
                error: String,
                base_version: Option<IdentityVersion>,
            },
            #[serde(rename_all = "camelCase")]
            Ping {},
        }
        let s: ServerMessageJson = serde_json::from_value(value)?;
        let result = match s {
            ServerMessageJson::Transition {
                start_version,
                end_version,
                modifications,
            } => ServerMessage::Transition {
                start_version: start_version.try_into()?,
                end_version: end_version.try_into()?,
                modifications: modifications
                    .into_iter()
                    .map(|sm: JsonValue| sm.try_into())
                    .collect::<anyhow::Result<Vec<StateModification<V>>>>()?,
            },
            ServerMessageJson::QueriesFailed { failures } => ServerMessage::QueriesFailed {
                failures: failures
                    .into_iter()
                    .map(QueryFailure::try_from)
                    .collect::<anyhow::Result<Vec<_>>>()?,
            },
            ServerMessageJson::MutationResponse {
                request_id,
                mutation_id,
                success,
                result,
                ts,
                log_lines,
            } => {
                let result = if success {
                    Ok(result.try_into()?)
                } else {
                    let msg: String = serde_json::from_value(result)?;
                    Err(msg)
                };
                let request_id = if let Some(request_id) = request_id {
                    request_id
                } else {
                    mutation_id.ok_or_else(|| {
                        anyhow::anyhow!("Either mutation_id or request_id must be set")
                    })?
                };
                ServerMessage::MutationResponse {
                    request_id,
                    result,
                    ts: ts
                        .map(|s| string_to_u64(&s))
                        .transpose()?
                        .map(Timestamp::try_from)
                        .transpose()?,
                    log_lines,
                }
            },
            ServerMessageJson::ActionResponse {
                request_id,
                action_id,
                success,
                result,
                log_lines,
            } => {
                let result = if success {
                    Ok(result.try_into()?)
                } else {
                    let msg: String = serde_json::from_value(result)?;
                    Err(msg)
                };
                let request_id = if let Some(request_id) = request_id {
                    request_id
                } else {
                    action_id.ok_or_else(|| {
                        anyhow::anyhow!("Either mutation_id or request_id must be set")
                    })?
                };
                ServerMessage::ActionResponse {
                    request_id,
                    result,
                    log_lines,
                }
            },
            ServerMessageJson::FatalError { error } => ServerMessage::FatalError {
                error_message: error,
            },
            ServerMessageJson::AuthError {
                error,
                base_version,
            } => ServerMessage::AuthError {
                error_message: error,
                base_version,
            },
            ServerMessageJson::Ping {} => ServerMessage::Ping {},
        };
        Ok(result)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserIdentityAttributesJson {
    // Always exists when serializing
    pub token_identifier: Option<UserIdentifier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub birthday: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone_number_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl TryFrom<JsonValue> for UserIdentityAttributes {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        let raw: UserIdentityAttributesJson = serde_json::from_value(value)?;
        let token_identifier = if let Some(token_identifier) = raw.token_identifier {
            token_identifier
        } else if let (Some(issuer), Some(subject)) = (&raw.issuer, &raw.subject) {
            UserIdentifier::construct(issuer, subject)
        } else {
            bail!("Either \"tokenIdentifier\" or \"issuer\" and \"subject\" must be set")
        };

        Ok(UserIdentityAttributes {
            token_identifier,
            issuer: raw.issuer,
            subject: raw.subject,
            name: raw.name,
            given_name: raw.given_name,
            family_name: raw.family_name,
            nickname: raw.nickname,
            preferred_username: raw.preferred_username,
            profile_url: raw.profile_url,
            picture_url: raw.picture_url,
            website_url: raw.website_url,
            email: raw.email,
            email_verified: raw.email_verified,
            gender: raw.gender,
            birthday: raw.birthday,
            timezone: raw.timezone,
            language: raw.language,
            phone_number: raw.phone_number,
            phone_number_verified: raw.phone_number_verified,
            address: raw.address,
            updated_at: raw.updated_at,
        })
    }
}

impl TryFrom<UserIdentityAttributes> for JsonValue {
    type Error = anyhow::Error;

    fn try_from(value: UserIdentityAttributes) -> Result<Self, Self::Error> {
        let raw = UserIdentityAttributesJson {
            token_identifier: Some(value.token_identifier),
            issuer: value.issuer,
            subject: value.subject,
            name: value.name,
            given_name: value.given_name,
            family_name: value.family_name,
            nickname: value.nickname,
            preferred_username: value.preferred_username,
            profile_url: value.profile_url,
            picture_url: value.picture_url,
            website_url: value.website_url,
            email: value.email,
            email_verified: value.email_verified,
            gender: value.gender,
            birthday: value.birthday,
            timezone: value.timezone,
            language: value.language,
            phone_number: value.phone_number,
            phone_number_verified: value.phone_number_verified,
            address: value.address,
            updated_at: value.updated_at,
        };
        Ok(serde_json::to_value(raw)?)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{
        self,
        json,
        Value as JsonValue,
    };

    use super::{
        string_to_u64,
        u64_to_string,
    };
    use crate::{
        testing::assert_roundtrips,
        ClientMessage,
        ServerMessage,
        UserIdentifier,
        UserIdentityAttributes,
    };

    #[derive(Clone, Debug, PartialEq, Eq, proptest_derive::Arbitrary)]
    pub struct TestValue(
        #[cfg_attr(
            any(test, feature = "testing"),
            proptest(strategy = "crate::testing::arb_json()")
        )]
        pub JsonValue,
    );

    impl From<TestValue> for JsonValue {
        fn from(v: TestValue) -> JsonValue {
            v.0
        }
    }
    impl TryFrom<JsonValue> for TestValue {
        type Error = anyhow::Error;

        fn try_from(v: JsonValue) -> anyhow::Result<TestValue> {
            Ok(TestValue(v))
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, .. ProptestConfig::default() })]

        #[test]
        fn proptest_u64_roundtrips(x in any::<u64>()) {
            assert_eq!(string_to_u64(&u64_to_string(x)).unwrap(), x);
        }

        #[test]
        fn proptest_client_message_roundtrips(m in any::<ClientMessage>()) {
            assert_roundtrips::<ClientMessage, JsonValue>(m);
        }

        #[test]
        fn proptest_server_message_roundtrips(m in any::<ServerMessage<TestValue>>()) {
            assert_roundtrips::<ServerMessage<TestValue>, JsonValue>(m);
        }

        #[test]
        fn proptest_user_identity_attributes_roundtrips(m in any::<UserIdentityAttributes>()) {
            assert_roundtrips::<UserIdentityAttributes, JsonValue>(m);
        }
    }

    #[test]
    fn authentication_token_backwards_compatability() {
        let old_admin_auth_message = json!({"type": "Authenticate", "tokenType": "Admin", "value": "fakefakefake", "baseVersion": 0});
        assert_roundtrips::<JsonValue, ClientMessage>(old_admin_auth_message);
        let old_user_auth_message = json!({"type": "Authenticate", "tokenType": "User", "value": "fakefakefake", "baseVersion": 0});
        assert_roundtrips::<JsonValue, ClientMessage>(old_user_auth_message);
    }

    #[test]
    fn user_identity_attributes_deserialize_token_identifier_given() {
        let serialized = "{\"tokenIdentifier\":\"fake_identifier\"}";
        let deserialized: UserIdentityAttributes = serde_json::from_str::<JsonValue>(serialized)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            deserialized.token_identifier,
            UserIdentifier("fake_identifier".to_owned())
        );
    }

    #[test]
    fn user_identity_attributes_deserialize_token_identifier_deriver() {
        let serialized = "{\"issuer\":\"fake_issuer\", \"subject\":\"fake_subject\"}";
        let deserialized: UserIdentityAttributes = serde_json::from_str::<JsonValue>(serialized)
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            deserialized.token_identifier,
            UserIdentifier::construct("fake_issuer", "fake_subject")
        );
    }

    #[test]
    fn user_identity_attributes_deserialize_token_identifier_cannot_derive() {
        let serialized = "{\"issuer\":\"fake_issuer\"}";
        let deserialized: anyhow::Result<UserIdentityAttributes> =
            serde_json::from_str::<JsonValue>(serialized)
                .unwrap()
                .try_into();
        assert!(deserialized
            .unwrap_err()
            .to_string()
            .contains("Either \"tokenIdentifier\" or \"issuer\" and \"subject\" must be set"));
    }
}
