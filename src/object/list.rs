// Copyright 2019 Joyent, Inc.

use std::vec::Vec;

use base64;
use serde_derive::{Deserialize, Serialize};
use serde_json::{Value, json};
use slog::{Logger, debug, error, warn};
use uuid::Uuid;

use cueball_postgres_connection::PostgresConnection;
use rust_fast::protocol::{FastMessage, FastMessageData};

use crate::object::{
    ObjectResponse,
    to_json
};
use crate::sql;
use crate::util::{
    HandlerError,
    HandlerResponse,
    array_wrap,
    other_error
};

const METHOD: &str = "listobjects";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ListObjectsPayload {
    pub owner      : Uuid,
    pub bucket_id  : Uuid,
    pub vnode      : u64,
    pub prefix     : Option<String>,
    pub limit      : u64,
    pub marker     : Option<String>,
    pub request_id : Uuid
}

pub(crate) fn handler(
    msg_id: u32,
    data: &Value,
    mut conn: &mut PostgresConnection,
    log: &Logger
) -> Result<HandlerResponse, HandlerError>
{
    debug!(log, "handling {} function request", &METHOD);

    serde_json::from_value::<Vec<ListObjectsPayload>>(data.clone())
        .map_err(|e| e.to_string())
        .and_then(|mut arr| {
            // Remove outer JSON array required by Fast
            if !arr.is_empty() {
                Ok(arr.remove(0))
            } else {
                let err_msg = "Failed to parse JSON data as payload for \
                               getobject function";
                warn!(log, "{}: {}", err_msg, data);
                Err(err_msg.to_string())
            }
        })
        .and_then(|payload| {
            // Make database request
            let req_id = payload.request_id;
            debug!(log, "parsed ListObjectsPayload, req_id: {}", &req_id);

            if payload.limit > 0 && payload.limit <= 1024 {
                list(msg_id, payload, &mut conn)
                    .and_then(|resp| {
                        // Handle the successful database response
                        debug!(log, "{} operation was successful, req_id: {}", &METHOD, &req_id);
                        Ok(HandlerResponse::from(resp))
                    })
                    .or_else(|e| {
                        // Handle database error response
                        error!(log, "{} operation failed: {}, req_id: {}", &METHOD, &e, &req_id);

                        // Database errors are returned to as regular Fast messages
                        // to be handled by the calling application
                        let value = array_wrap(json!({
                            "name": "PostgresError",
                            "message": e
                        }));
                        let msg_data = FastMessageData::new(METHOD.into(), value);
                        let msg: HandlerResponse =
                            FastMessage::data(msg_id, msg_data).into();
                        Ok(msg)
                    })
            } else {
                // Limit constraint violations are returned to as regular
                // Fast messages to be handled by the calling application
                let e = format!("the {} limit option must be a value between 1 \
                                 and 1024. the requested limit was {}, req_id: \
                                 {}", &METHOD, &payload.limit, &req_id);
                let value = array_wrap(json!({
                    "name": "LimitConstraintError",
                    "message": e
                }));
                let msg_data = FastMessageData::new(METHOD.into(), value);
                let msg: HandlerResponse =
                    FastMessage::data(msg_id, msg_data).into();
                Ok(msg)
            }
        })
        .map_err(|e| HandlerError::IO(other_error(&e)))
}

fn list(
    msg_id: u32,
    payload: ListObjectsPayload,
    mut conn: &mut PostgresConnection
) -> Result<Vec<FastMessage>, String>
{
    let query_result =
        match (payload.marker, payload.prefix) {
            (Some(marker), Some(prefix)) => {
                let sql = list_sql_prefix_marker(payload.vnode, payload.limit);
                let prefix = format!("{}%", prefix);
                sql::query(sql::Method::ObjectList, &mut conn, sql.as_str(),
                           &[&payload.owner, &payload.bucket_id, &prefix, &marker])
            }
            (Some(marker), None) => {
                let sql = list_sql_marker(payload.vnode, payload.limit);
                sql::query(sql::Method::ObjectList, &mut conn, sql.as_str(),
                           &[&payload.owner, &payload.bucket_id, &marker])
            }
            (None, Some(prefix)) => {
                let sql = list_sql_prefix(payload.vnode, payload.limit);
                let prefix = format!("{}%", prefix);
                sql::query(sql::Method::ObjectList, &mut conn, sql.as_str(),
                           &[&payload.owner, &payload.bucket_id, &prefix])
            }
            (None, None) => {
                let sql = list_sql(payload.vnode, payload.limit);
                sql::query(sql::Method::ObjectList, &mut conn, sql.as_str(),
                           &[&payload.owner, &payload.bucket_id])
            }
        };

    let mut msgs: Vec<FastMessage> = Vec::with_capacity(1024);

    query_result
        .map_err(|e| e.to_string())
        .and_then(|rows| {
            for row in rows.iter() {
                let content_md5_bytes: Vec<u8> = row.get(7);
                let content_md5 = base64::encode(&content_md5_bytes);
                let resp = ObjectResponse {
                    id             : row.get("id"),
                    owner          : row.get("owner"),
                    bucket_id      : row.get("bucket_id"),
                    name           : row.get("name"),
                    created        : row.get("created"),
                    modified       : row.get("modified"),
                    content_length : row.get("content_length"),
                    content_md5,
                    content_type   : row.get("content_type"),
                    headers        : row.get("headers"),
                    sharks         : row.get("sharks"),
                    properties     : row.get("properties")
                };

                let value = to_json(resp);
                let msg_data =
                    FastMessageData::new(METHOD.into(), array_wrap(value));
                let msg = FastMessage::data(msg_id, msg_data);

                msgs.push(msg);
            }
            Ok(msgs)
        })
}

fn list_sql_prefix_marker(vnode: u64, limit: u64) -> String {
    format!("SELECT id, owner, bucket_id, name, created, modified, \
        content_length, content_md5, content_type, headers, sharks, \
        properties \
        FROM manta_bucket_{}.manta_bucket_object
        WHERE owner = $1 AND bucket_id = $2 AND name like $3 AND name > $4
        ORDER BY name ASC
        LIMIT {}",
        vnode, limit)
}

fn list_sql_prefix(vnode: u64, limit: u64) -> String {
    format!("SELECT id, owner, bucket_id, name, created, modified, \
        content_length, content_md5, content_type, headers, sharks, \
        properties \
        FROM manta_bucket_{}.manta_bucket_object
        WHERE owner = $1 AND bucket_id = $2 AND name like $3
        ORDER BY name ASC
        LIMIT {}",
        vnode, limit)
}

fn list_sql_marker(vnode: u64, limit: u64) -> String {
    format!("SELECT id, owner, bucket_id, name, created, modified, \
        content_length, content_md5, content_type, headers, sharks, \
        properties \
        FROM manta_bucket_{}.manta_bucket_object
        WHERE owner = $1 AND bucket_id = $2 AND name > $3
        ORDER BY name ASC
        LIMIT {}",
        vnode, limit)
}

fn list_sql(vnode: u64, limit: u64) -> String {
    format!("SELECT id, owner, bucket_id, name, created, modified, \
        content_length, content_md5, content_type, headers, sharks, \
        properties \
        FROM manta_bucket_{}.manta_bucket_object
        WHERE owner = $1 AND bucket_id = $2
        ORDER BY name ASC
        LIMIT {}",
        vnode, limit)
}


#[cfg(test)]
mod test {
    use super::*;

    use quickcheck::{quickcheck, Arbitrary, Gen};
    use quickcheck_helpers::random;
    use serde_json;
    use serde_json::Map;

    #[derive(Clone, Debug)]
    struct ListObjectsJson(Value);

    impl Arbitrary for ListObjectsJson {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            let owner = serde_json::to_value(Uuid::new_v4())
                .expect("failed to convert owner field to Value");
            let bucket_id = serde_json::to_value(Uuid::new_v4())
                .expect("failed to convert bucket_id field to Value");
            let vnode = serde_json::to_value(u64::arbitrary(g))
                .expect("failed to convert vnode field to Value");
            let prefix = serde_json::to_value(random::string(g, 32))
                .expect("failed to convert prefix field to Value");
            let limit = serde_json::to_value(u64::arbitrary(g))
                .expect("failed to convert limit field to Value");
            let marker = serde_json::to_value(random::string(g, 32))
                .expect("failed to convert marker field to Value");
            let request_id = serde_json::to_value(Uuid::new_v4())
                .expect("failed to convert request_id field to Value");

            let mut obj = Map::new();
            obj.insert("owner".into(), owner);
            obj.insert("bucket_id".into(), bucket_id);
            obj.insert("vnode".into(), vnode);
            obj.insert("prefix".into(), prefix);
            obj.insert("limit".into(), limit);
            obj.insert("marker".into(), marker);
            obj.insert("request_id".into(), request_id);
            ListObjectsJson(Value::Object(obj))
        }
    }

    impl Arbitrary for ListObjectsPayload {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            let owner = Uuid::new_v4();
            let bucket_id = Uuid::new_v4();
            let vnode = u64::arbitrary(g);
            let prefix = Some(random::string(g, 32));
            let limit = u64::arbitrary(g);
            let marker = Some(random::string(g, 32));
            let request_id = Uuid::new_v4();

            ListObjectsPayload {
                owner,
                bucket_id,
                vnode,
                prefix,
                limit,
                marker,
                request_id
            }
        }
    }

    quickcheck! {
        fn prop_list_object_payload_roundtrip(msg: ListObjectsPayload) -> bool {
            match serde_json::to_string(&msg) {
                Ok(list_str) => {
                    let decode_result: Result<ListObjectsPayload, _> =
                        serde_json::from_str(&list_str);
                    match decode_result {
                        Ok(decoded_msg) => decoded_msg == msg,
                        Err(_) => false
                    }
                },
                Err(_) => false
            }
        }
    }

    quickcheck! {
        fn prop_listobject_payload_from_json(json: ListObjectsJson) -> bool {
            let decode_result1: Result<ListObjectsPayload, _> =
                serde_json::from_value(json.0.clone());
            let res1 = decode_result1.is_ok();

            let decode_result2: Result<Vec<ListObjectsPayload>, _> =
                serde_json::from_value(Value::Array(vec![json.0]));
            let res2 = decode_result2.is_ok();

            res1 && res2
        }
    }
}