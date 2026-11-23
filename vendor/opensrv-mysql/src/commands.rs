// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::myc::constants::{CapabilityFlags, Command as CommandByte};

#[derive(Debug)]
pub struct ClientHandshake {
    #[allow(dead_code)]
    pub(crate) maxps: u32,
    pub(crate) capabilities: CapabilityFlags,
    #[allow(dead_code)]
    pub(crate) collation: u16,
    #[allow(dead_code)]
    pub(crate) db: Option<Vec<u8>>,
    pub(crate) username: Option<Vec<u8>>,
    pub(crate) auth_response: Vec<u8>,
    pub(crate) auth_plugin: Vec<u8>,
}

#[allow(clippy::branches_sharing_code)]
pub fn client_handshake(i: &[u8], after_tls: bool) -> nom::IResult<&[u8], ClientHandshake> {
    // mysql handshake protocol documentation
    // https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase_packets_protocol_handshake_response.html
    let (i, cap) = nom::number::complete::le_u16(i)?;

    let mut capabilities = CapabilityFlags::from_bits_truncate(cap as u32);
    if capabilities.contains(CapabilityFlags::CLIENT_PROTOCOL_41) {
        // HandshakeResponse41
        let (i, cap2) = nom::number::complete::le_u16(i)?;
        let cap = (cap2 as u32) << 16 | cap as u32;

        capabilities = CapabilityFlags::from_bits_truncate(cap);

        let (i, maxps) = nom::number::complete::le_u32(i)?;
        let (i, collation) = nom::bytes::complete::take(1u8)(i)?;

        let (i, _) = nom::bytes::complete::take(23u8)(i)?;

        if !after_tls && capabilities.contains(CapabilityFlags::CLIENT_SSL) {
            return Ok((
                i,
                ClientHandshake {
                    capabilities,
                    maxps,
                    collation: u16::from(collation[0]),
                    username: None,
                    db: None,
                    auth_response: vec![],
                    auth_plugin: vec![],
                },
            ));
        }

        let (i, username) = if after_tls || !capabilities.contains(CapabilityFlags::CLIENT_SSL) {
            let (i, user) = nom::bytes::complete::take_until(&b"\0"[..])(i)?;
            let (i, _) = nom::bytes::complete::tag(b"\0")(i)?;
            (i, Some(user.to_owned()))
        } else {
            (i, None)
        };

        let (i, auth_response) =
            if capabilities.contains(CapabilityFlags::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA) {
                let (i, size) = read_length_encoded_number(i)?;
                nom::bytes::complete::take(size)(i)?
            } else if capabilities.contains(CapabilityFlags::CLIENT_SECURE_CONNECTION) {
                let (i, size) = nom::number::complete::le_u8(i)?;
                nom::bytes::complete::take(size)(i)?
            } else {
                nom::bytes::complete::take_until(&b"\0"[..])(i)?
            };

        let (i, db) =
            if capabilities.contains(CapabilityFlags::CLIENT_CONNECT_WITH_DB) && !i.is_empty() {
                let (i, db) = nom::bytes::complete::take_until(&b"\0"[..])(i)?;
                let (i, _) = nom::bytes::complete::tag(b"\0")(i)?;
                (i, Some(db))
            } else {
                (i, None)
            };

        let (i, auth_plugin) =
            if capabilities.contains(CapabilityFlags::CLIENT_PLUGIN_AUTH) && !i.is_empty() {
                let (i, auth_plugin) = nom::bytes::complete::take_until(&b"\0"[..])(i)?;

                let (i, _) = nom::bytes::complete::tag(b"\0")(i)?;
                (i, auth_plugin)
            } else {
                (i, &b""[..])
            };

        let mut i = i;
        if capabilities.contains(CapabilityFlags::CLIENT_CONNECT_ATTRS) && !i.is_empty() {
            if let Ok((remaining, attrs_len)) = read_length_encoded_number(i) {
                let take_len = std::cmp::min(attrs_len as usize, remaining.len());
                i = &remaining[take_len..];
            }
        }

        Ok((
            i,
            ClientHandshake {
                capabilities,
                maxps,
                collation: u16::from(collation[0]),
                username,
                db: db.map(|c| c.to_vec()),
                auth_response: auth_response.to_vec(),
                auth_plugin: auth_plugin.to_vec(),
            },
        ))
    } else {
        // HandshakeResponse320
        let (i, maxps1) = nom::number::complete::le_u16(i)?;
        let (i, maxps2) = nom::number::complete::le_u8(i)?;
        let maxps = (maxps2 as u32) << 16 | maxps1 as u32;
        let (i, username) = nom::bytes::complete::take_until(&b"\0"[..])(i)?;
        let (i, _) = nom::bytes::complete::tag(b"\0")(i)?;

        let (i, auth_response, db) =
            if capabilities.contains(CapabilityFlags::CLIENT_CONNECT_WITH_DB) {
                let (i, auth_response) = nom::bytes::complete::tag(b"\0")(i)?;
                let (i, _) = nom::bytes::complete::tag(b"\0")(i)?;

                let (i, db) = nom::bytes::complete::tag(b"\0")(i)?;
                let (i, _) = nom::bytes::complete::tag(b"\0")(i)?;

                (i, auth_response, Some(db))
            } else {
                (&b""[..], i, None)
            };

        Ok((
            i,
            ClientHandshake {
                capabilities,
                maxps,
                collation: 0,
                username: Some(username.to_vec()),
                db: db.map(|c| c.to_vec()),
                auth_response: auth_response.to_vec(),
                auth_plugin: vec![],
            },
        ))
    }
}

fn read_length_encoded_number(i: &[u8]) -> nom::IResult<&[u8], u64> {
    let (i, b) = nom::number::complete::le_u8(i)?;
    let size: usize = match b {
        0xfb => return Ok((i, 0)),
        0xfc => 2,
        0xfd => 3,
        0xfe => 8,
        _ => return Ok((i, b as u64)),
    };
    let mut bytes = [0u8; 8];
    let (i, b) = nom::bytes::complete::take(size)(i)?;
    bytes[..size].copy_from_slice(b);
    Ok((i, u64::from_le_bytes(bytes)))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Query {
        sql: &'a [u8],
        attributes: Vec<(&'a [u8], &'a [u8])>,
    },
    ListFields(&'a [u8]),
    Close(u32),
    Prepare(&'a [u8]),
    Init(&'a [u8]),
    Execute {
        stmt: u32,
        flags: u8,
        query_attributes: bool,
        params: &'a [u8],
    },
    SendLongData {
        stmt: u32,
        param: u16,
        data: &'a [u8],
    },
    ResetStatement(u32),
    ResetConnection,
    Ping,
    Quit,
}

pub fn execute(i: &[u8], capabilities: CapabilityFlags) -> nom::IResult<&[u8], Command<'_>> {
    let (i, stmt) = nom::number::complete::le_u32(i)?;
    let (i, flags) = nom::number::complete::le_u8(i)?;
    let (i, _iterations) = nom::number::complete::le_u32(i)?;
    Ok((
        &[],
        Command::Execute {
            stmt,
            flags,
            query_attributes: capabilities.contains(CapabilityFlags::CLIENT_QUERY_ATTRIBUTES),
            params: i,
        },
    ))
}

pub fn send_long_data(i: &[u8]) -> nom::IResult<&[u8], Command<'_>> {
    let (i, stmt) = nom::number::complete::le_u32(i)?;
    let (i, param) = nom::number::complete::le_u16(i)?;
    Ok((
        &[],
        Command::SendLongData {
            stmt,
            param,
            data: i,
        },
    ))
}

/// Parse MySQL query attributes (CLIENT_QUERY_ATTRIBUTES). The text protocol
/// payload is `count`, then `name_len/name/value_len/value` pairs, then SQL.
/// A malformed attribute block leaves the payload untouched so the backend
/// rejects it instead of executing a truncated statement.
fn parse_query_payload(i: &[u8], capabilities: CapabilityFlags) -> (&[u8], Vec<(&[u8], &[u8])>) {
    if !capabilities.contains(CapabilityFlags::CLIENT_QUERY_ATTRIBUTES) {
        return (i, Vec::new());
    }
    let Ok((mut rest, count)) = read_length_encoded_number(i) else {
        return (i, Vec::new());
    };
    if count == 0 {
        return (rest, Vec::new());
    }
    let mut attributes = Vec::with_capacity((count as usize).min(32));
    for _ in 0..count {
        let Ok((next, name_len)) = read_length_encoded_number(rest) else {
            return (i, Vec::new());
        };
        let Ok((next, name)) =
            nom::bytes::complete::take::<_, _, nom::error::Error<&[u8]>>(name_len as usize)(next)
        else {
            return (i, Vec::new());
        };
        let Ok((next, value_len)) = read_length_encoded_number(next) else {
            return (i, Vec::new());
        };
        let Ok((next, value)) =
            nom::bytes::complete::take::<_, _, nom::error::Error<&[u8]>>(value_len as usize)(next)
        else {
            return (i, Vec::new());
        };
        attributes.push((name, value));
        rest = next;
    }
    (rest, attributes)
}

pub fn parse(i: &[u8], client_capabilities: CapabilityFlags) -> nom::IResult<&[u8], Command<'_>> {
    use nom::bytes::complete::tag;
    use nom::combinator::{map, rest};
    use nom::sequence::preceded;
    nom::branch::alt((
        map(
            preceded(tag(&[CommandByte::COM_QUERY as u8]), rest),
            |i: &[u8]| {
                let (sql, attributes) = parse_query_payload(i, client_capabilities);
                Command::Query { sql, attributes }
            },
        ),
        map(
            preceded(tag(&[CommandByte::COM_FIELD_LIST as u8]), rest),
            Command::ListFields,
        ),
        map(
            preceded(tag(&[CommandByte::COM_INIT_DB as u8]), rest),
            Command::Init,
        ),
        map(
            preceded(tag(&[CommandByte::COM_STMT_PREPARE as u8]), rest),
            Command::Prepare,
        ),
        preceded(tag(&[CommandByte::COM_STMT_EXECUTE as u8]), |i| {
            execute(i, client_capabilities)
        }),
        preceded(
            tag(&[CommandByte::COM_STMT_SEND_LONG_DATA as u8]),
            send_long_data,
        ),
        map(
            preceded(
                tag(&[CommandByte::COM_STMT_RESET as u8]),
                nom::number::complete::le_u32,
            ),
            Command::ResetStatement,
        ),
        map(
            preceded(
                tag(&[CommandByte::COM_STMT_CLOSE as u8]),
                nom::number::complete::le_u32,
            ),
            Command::Close,
        ),
        map(tag(&[CommandByte::COM_RESET_CONNECTION as u8]), |_| {
            Command::ResetConnection
        }),
        map(tag(&[CommandByte::COM_QUIT as u8]), |_| Command::Quit),
        map(tag(&[CommandByte::COM_PING as u8]), |_| Command::Ping),
    ))(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_attributes() -> CapabilityFlags {
        CapabilityFlags::CLIENT_PROTOCOL_41 | CapabilityFlags::CLIENT_QUERY_ATTRIBUTES
    }

    #[test]
    fn execute_parses_parameter_count_and_attribute_names() {
        let traceparent = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let mut packet = vec![CommandByte::COM_STMT_EXECUTE as u8];
        packet.extend_from_slice(&7u32.to_le_bytes());
        packet.push(0x08); // PARAMETER_COUNT_AVAILABLE
        packet.extend_from_slice(&1u32.to_le_bytes());
        packet.push(1); // parameter_count
        packet.push(0); // null bitmap
        packet.push(1); // new_params_bind_flag
        packet.push(0xfe); // MYSQL_TYPE_STRING
        packet.push(0x00);
        packet.push(14); // name length
        packet.extend_from_slice(b"traceparent");
        packet.push(traceparent.len() as u8);
        packet.extend_from_slice(traceparent);

        let (_, command) = parse(&packet, query_attributes()).expect("parse execute");
        match command {
            Command::Execute {
                stmt,
                flags,
                query_attributes,
                params,
            } => {
                assert_eq!(stmt, 7);
                assert_eq!(flags, 0x08);
                assert!(query_attributes);
                assert_eq!(params[0], 1);
                assert!(params.len() > traceparent.len());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn execute_without_parameter_count_keeps_legacy_payload() {
        let mut packet = vec![CommandByte::COM_STMT_EXECUTE as u8];
        packet.extend_from_slice(&9u32.to_le_bytes());
        packet.push(0);
        packet.extend_from_slice(&1u32.to_le_bytes());
        packet.extend_from_slice(&[0u8; 3]);

        let (_, command) = parse(&packet, query_attributes()).expect("parse execute");
        match command {
            Command::Execute { params, flags, .. } => {
                assert_eq!(flags, 0);
                assert_eq!(params, &[0u8; 3]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
