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

use std::collections::HashMap;

use crate::myc;
use crate::{StatementData, Value};

fn read_lenenc(input: &mut &[u8]) -> Option<u64> {
    let first = *input.first()?;
    *input = &input[1..];
    let size = match first {
        0xfc => 2,
        0xfd => 3,
        0xfe => 8,
        _ => return Some(u64::from(first)),
    };
    let (bytes, rest) = input.split_at_checked(size)?;
    *input = rest;
    let mut value = [0u8; 8];
    value[..size].copy_from_slice(bytes);
    Some(u64::from_le_bytes(value))
}

/// A `ParamParser` decodes query parameters included in a client's `EXECUTE` command given
/// type information for the expected parameters.
///
/// Users should invoke [`iter`](struct.ParamParser.html#method.iter) method to iterate over the
/// provided parameters.
pub struct ParamParser<'a> {
    pub(crate) params: u16,
    pub(crate) positional_params: u16,
    pub(crate) query_attributes: bool,
    pub(crate) bytes: &'a [u8],
    pub(crate) long_data: &'a HashMap<u16, Vec<u8>>,
    pub(crate) bound_types: &'a mut Vec<(myc::constants::ColumnType, bool)>,
}

impl<'a> ParamParser<'a> {
    pub(crate) fn new(
        mut input: &'a [u8],
        stmt: &'a mut StatementData,
        flags: u8,
        query_attributes: bool,
    ) -> Self {
        let parameter_count = if query_attributes
            && (stmt.params > 0 || flags & 0x08 != 0)
        {
            read_lenenc(&mut input).map(|count| count as u16)
        } else {
            None
        };
        ParamParser {
            params: parameter_count.unwrap_or(stmt.params),
            positional_params: stmt.params,
            query_attributes,
            bytes: input,
            long_data: &stmt.long_data,
            bound_types: &mut stmt.bound_types,
        }
    }
}

impl<'a> IntoIterator for ParamParser<'a> {
    type IntoIter = Params<'a>;
    type Item = ParamValue<'a>;
    fn into_iter(self) -> Params<'a> {
        Params {
            params: self.params,
            positional_params: self.positional_params,
            query_attributes: self.query_attributes,
            input: self.bytes,
            nullmap: None,
            col: 0,
            long_data: self.long_data,
            bound_types: self.bound_types,
            names: Vec::new(),
        }
    }
}

/// An iterator over parameters provided by a client in an `EXECUTE` command.
pub struct Params<'a> {
    params: u16,
    positional_params: u16,
    query_attributes: bool,
    input: &'a [u8],
    nullmap: Option<&'a [u8]>,
    col: u16,
    long_data: &'a HashMap<u16, Vec<u8>>,
    bound_types: &'a mut Vec<(myc::constants::ColumnType, bool)>,
    names: Vec<Option<&'a [u8]>>,
}

/// A single parameter value provided by a client when issuing an `EXECUTE` command.
pub struct ParamValue<'a> {
    /// The value provided for this parameter.
    pub value: Value<'a>,
    /// The column type assigned to this parameter.
    pub coltype: myc::constants::ColumnType,
    /// Name supplied for a query attribute, if any.
    pub name: Option<&'a [u8]>,
}

impl<'a> Iterator for Params<'a> {
    type Item = ParamValue<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.nullmap.is_none() {
            let nullmap_len = (self.params as usize).div_ceil(8);
            let (nullmap, rest) = self.input.split_at(nullmap_len);
            self.nullmap = Some(nullmap);
            self.input = rest;

            if !rest.is_empty() && rest[0] != 0x00 {
                self.input = &rest[1..];
                self.bound_types.clear();
                self.names.clear();
                for _ in 0..self.params {
                    let (typ, rest) = self.input.split_at(2);
                    self.input = rest;
                    self.bound_types.push((
                        myc::constants::ColumnType::try_from(typ[0]).unwrap_or_else(|e| {
                            panic!("bad column type 0x{:x}: {}", typ[0], e)
                        }),
                        (typ[1] & 128) != 0,
                    ));
                    let name = if self.query_attributes {
                        read_lenenc(&mut self.input)
                            .and_then(|len| {
                                let (name, rest) = self.input.split_at(len as usize);
                                self.input = rest;
                                Some(name)
                            })
                            .filter(|name| !name.is_empty())
                    } else {
                        None
                    };
                    self.names.push(name);
                }
            }
        }

        if self.col >= self.params {
            return None;
        }
        let pt = self
            .bound_types
            .get(self.col as usize)
            .copied()
            .unwrap_or((myc::constants::ColumnType::MYSQL_TYPE_STRING, false));
        let name = self
            .names
            .get(self.col as usize)
            .copied()
            .flatten()
            .filter(|_| self.col < self.positional_params || self.query_attributes);

        // https://web.archive.org/web/20170404144156/https://dev.mysql.com/doc/internals/en/null-bitmap.html
        // NULL-bitmap-byte = ((field-pos + offset) / 8)
        // NULL-bitmap-bit  = ((field-pos + offset) % 8)
        if let Some(nullmap) = self.nullmap {
            let byte = self.col as usize / 8;
            if byte >= nullmap.len() {
                return None;
            }
            if (nullmap[byte] & 1u8 << (self.col % 8)) != 0 {
                self.col += 1;
                return Some(ParamValue {
                    value: Value::null(),
                    coltype: pt.0,
                    name,
                });
            }
        } else {
            unreachable!();
        }

        let v = if let Some(data) = self.long_data.get(&self.col) {
            Value::bytes(&data[..])
        } else {
            Value::parse_from(&mut self.input, pt.0, pt.1).unwrap()
        };
        self.col += 1;
        Some(ParamValue {
            value: v,
            coltype: pt.0,
            name,
        })
    }
}
