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

use std::borrow::Borrow;
use std::collections::HashMap;
use std::io::{self, Write};

use byteorder::WriteBytesExt;
use mysql_common::constants::{CapabilityFlags, ColumnFlags, StatusFlags};
use tokio::io::AsyncWrite;

use crate::packet_writer::PacketWriter;
use crate::value::ToMysqlValue;
use crate::{writers, OkResponse};
use crate::{Column, ErrorKind, StatementData};

/// Convenience type for responding to a client `USE <db>` command.
pub struct InitWriter<'a, W> {
    pub(crate) client_capabilities: CapabilityFlags,
    pub(crate) writer: &'a mut PacketWriter<W>,
}

impl<'a, W: 'a + AsyncWrite + Unpin> InitWriter<'a, W> {
    /// Tell client that database context has been changed
    pub async fn ok(self) -> io::Result<()> {
        writers::write_ok_packet(self.writer, self.client_capabilities, OkResponse::default()).await
    }

    /// Tell client that there was a problem changing the database context.
    /// Although you can return any valid MySQL error code you probably want
    /// to keep it similar to the MySQL server and issue either a
    /// `ErrorKind::ER_BAD_DB_ERROR` or a `ErrorKind::ER_DBACCESS_DENIED_ERROR`.
    pub async fn error<E>(self, kind: ErrorKind, msg: &E) -> io::Result<()>
    where
        E: Borrow<[u8]> + ?Sized,
    {
        writers::write_err(kind, msg.borrow(), self.writer).await
    }
}

/// Convenience type for responding to a client `PREPARE` command.
///
/// This type should not be dropped without calling
/// [`reply`](struct.StatementMetaWriter.html#method.reply) or
/// [`error`](struct.StatementMetaWriter.html#method.error).
#[must_use]
pub struct StatementMetaWriter<'a, W> {
    pub(crate) writer: &'a mut PacketWriter<W>,
    pub(crate) stmts: &'a mut HashMap<u32, StatementData>,
    pub(crate) client_capabilities: CapabilityFlags,
}

impl<'a, W: AsyncWrite + Unpin + 'a> StatementMetaWriter<'a, W> {
    /// Reply to the client with the given meta-information.
    ///
    /// `id` is a statement identifier that the client should supply when it later wants to execute
    /// this statement. `params` is a set of [`Column`](struct.Column.html) descriptors for the
    /// parameters the client must provide when executing the prepared statement. `columns` is a
    /// second set of [`Column`](struct.Column.html) descriptors for the values that will be
    /// returned in each row then the statement is later executed.
    pub async fn reply<PI, CI>(self, id: u32, params: PI, columns: CI) -> io::Result<()>
    where
        PI: IntoIterator<Item = &'a Column>,
        CI: IntoIterator<Item = &'a Column>,
        <PI as IntoIterator>::IntoIter: ExactSizeIterator,
        <CI as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        let params = params.into_iter();
        self.stmts.insert(
            id,
            StatementData {
                params: params.len() as u16,
                ..Default::default()
            },
        );
        writers::write_prepare_ok(id, params, columns, self.writer, self.client_capabilities).await
    }

    /// Reply to the client's `PREPARE` with an error.
    pub async fn error<E>(self, kind: ErrorKind, msg: &E) -> io::Result<()>
    where
        E: Borrow<[u8]> + ?Sized,
    {
        writers::write_err(kind, msg.borrow(), self.writer).await
    }
}

enum Finalizer {
    Ok(OkResponse),
    Eof,
}

/// Convenience type for providing query results to clients.
///
/// This type should not be dropped without calling
/// [`start`](struct.QueryResultWriter.html#method.start),
/// [`completed`](struct.QueryResultWriter.html#method.completed), or
/// [`error`](struct.QueryResultWriter.html#method.error).
///
/// To send multiple resultsets, use
/// [`RowWriter::finish_one`](struct.RowWriter.html#method.finish_one) and
/// [`complete_one`](struct.QueryResultWriter.html#method.complete_one). These are similar to
/// `RowWriter::finish` and `completed`, but both eventually yield back the `QueryResultWriter` so
/// that another resultset can be sent. To indicate that no more resultset will be sent, call
/// [`no_more_results`](struct.QueryResultWriter.html#method.no_more_results). All methods on
/// `QueryResultWriter` (except `no_more_results`) automatically start a new resultset. The
#[must_use]
pub struct QueryResultWriter<'a, W> {
    // XXX: specialization instead?
    pub(crate) is_bin: bool,
    pub(crate) client_capabilities: CapabilityFlags,
    pub(crate) writer: &'a mut PacketWriter<W>,
    last_end: Option<Finalizer>,
}

impl<'a, W: AsyncWrite + Unpin> QueryResultWriter<'a, W> {
    pub(crate) fn new(
        writer: &'a mut PacketWriter<W>,
        is_bin: bool,
        client_capabilities: CapabilityFlags,
    ) -> Self {
        QueryResultWriter {
            is_bin,
            client_capabilities,
            writer,
            last_end: None,
        }
    }

    async fn finalize(&mut self, more_exists: bool) -> io::Result<()> {
        let mut status = StatusFlags::empty();
        if more_exists {
            status.set(StatusFlags::SERVER_MORE_RESULTS_EXISTS, true);
        }
        match self.last_end.take() {
            None => Ok(()),
            Some(Finalizer::Ok(mut ok_packet)) => {
                if more_exists {
                    ok_packet
                        .status_flags
                        .insert(StatusFlags::SERVER_MORE_RESULTS_EXISTS);
                }
                writers::write_ok_packet(self.writer, self.client_capabilities, ok_packet).await
            }
            Some(Finalizer::Eof) => writers::write_eof_packet(self.writer, status).await,
        }
    }

    /// Start a resultset response to the client that conforms to the given `columns`.
    ///
    /// Note that if no columns are emitted, any written rows are ignored.
    ///
    /// See [`RowWriter`](struct.RowWriter.html).
    pub async fn start(mut self, columns: &'a [Column]) -> io::Result<RowWriter<'a, W>> {
        self.finalize(true).await?;
        RowWriter::new(self, columns, StatusFlags::empty()).await
    }

    /// Start the special result set carrying prepared CALL OUT/INOUT parameters.
    pub async fn start_out_params(mut self, columns: &'a [Column]) -> io::Result<RowWriter<'a, W>> {
        self.finalize(true).await?;
        RowWriter::new(
            self,
            columns,
            StatusFlags::SERVER_PS_OUT_PARAMS | StatusFlags::SERVER_MORE_RESULTS_EXISTS,
        )
        .await
    }

    /// Whether the client negotiated prepared-statement multi-result support.
    pub fn supports_ps_multi_results(&self) -> bool {
        self.client_capabilities
            .contains(CapabilityFlags::CLIENT_PS_MULTI_RESULTS)
    }

    /// Send an empty resultset response to the client indicating that `rows` rows were affected by
    /// the query in this resultset. `last_insert_id` may be given to communiate an identifier for
    /// a client's most recent insertion.
    pub async fn complete_one(
        mut self,
        ok_packet: OkResponse,
    ) -> io::Result<QueryResultWriter<'a, W>> {
        self.finalize(true).await?;
        self.last_end = Some(Finalizer::Ok(ok_packet));
        Ok(self)
    }

    /// Send an empty resultset response to the client indicating that `rows` rows were affected by
    /// the query. `last_insert_id` may be given to communiate an identifier for a client's most
    /// recent insertion.
    pub async fn completed(self, ok_packet: OkResponse) -> io::Result<()> {
        self.complete_one(ok_packet).await?.no_more_results().await
    }

    /// Reply to the client's query with an error.
    pub async fn error<E>(mut self, kind: ErrorKind, msg: &E) -> io::Result<()>
    where
        E: Borrow<[u8]> + ?Sized,
    {
        self.finalize(true).await?;
        writers::write_err(kind, msg.borrow(), self.writer).await
    }

    /// Reply with an arbitrary protocol error number and SQLSTATE.
    pub async fn error_with_code<E>(
        mut self,
        error_code: u16,
        sqlstate: &[u8; 5],
        msg: &E,
    ) -> io::Result<()>
    where
        E: Borrow<[u8]> + ?Sized,
    {
        self.finalize(true).await?;
        writers::write_err_raw(error_code, sqlstate, msg.borrow(), self.writer).await
    }

    /// Send the last bits of the last resultset to the client, and indicate that there are no more
    /// resultsets coming.
    pub async fn no_more_results(mut self) -> io::Result<()> {
        self.finalize(false).await
    }
}

/// Convenience type for sending rows of a resultset to a client.
///
/// Rows can either be written out one column at a time (using
/// [`write_col`](struct.RowWriter.html#method.write_col) and
/// [`end_row`](struct.RowWriter.html#method.end_row)), or one row at a time (using
/// [`write_row`](struct.RowWriter.html#method.write_row)).
///
/// This type *may* be dropped without calling
/// [`write_row`](struct.RowWriter.html#method.write_row) or
/// [`finish`](struct.RowWriter.html#method.finish). However, in this case, the program may panic
/// if an I/O error occurs when sending the end-of-records marker to the client. To avoid this,
/// call [`finish`](struct.RowWriter.html#method.finish) explicitly.
#[must_use]
pub struct RowWriter<'a, W: AsyncWrite + Unpin> {
    client_capabilities: CapabilityFlags,
    result: Option<QueryResultWriter<'a, W>>,
    bitmap_len: usize,
    data: Vec<u8>,
    columns: &'a [Column],

    // next column to write for the current row
    // NOTE: (ab)used to track number of *rows* for a zero-column resultset
    col: usize,
    finished: bool,
}

impl<'a, W> RowWriter<'a, W>
where
    W: 'a + AsyncWrite + Unpin,
{
    async fn new(
        result: QueryResultWriter<'a, W>,
        columns: &'a [Column],
        metadata_status: StatusFlags,
    ) -> io::Result<RowWriter<'a, W>> {
        let bitmap_len = (columns.len() + 7 + 2) / 8;
        let client_capabilities = result.client_capabilities;
        let mut rw = RowWriter {
            client_capabilities,
            result: Some(result),
            columns,
            bitmap_len,
            data: Vec::new(),

            col: 0,

            finished: false,
        };
        rw.start(metadata_status).await?;
        Ok(rw)
    }

    #[inline]
    async fn start(&mut self, metadata_status: StatusFlags) -> io::Result<()> {
        if !self.columns.is_empty() {
            writers::column_definitions(
                self.columns,
                self.result.as_mut().unwrap().writer,
                self.client_capabilities,
                metadata_status,
            )
            .await?;
        }

        Ok(())
    }

    /// Write a value to the next column of the current row as a part of this resultset.
    ///
    /// If you do not call [`end_row`](struct.RowWriter.html#method.end_row) after the last row,
    /// any errors that occur when writing out the last row will be returned by
    /// [`finish`](struct.RowWriter.html#method.finish). If you do not call `finish` either, any
    /// errors will cause a panic when the `RowWriter` is dropped.
    ///
    /// Note that the row *must* conform to the column specification provided to
    /// [`QueryResultWriter::start`](struct.QueryResultWriter.html#method.start). If it does not,
    /// this method will return an error indicating that an invalid value type or specification was
    /// provided.
    pub fn write_col<T>(&mut self, v: T) -> io::Result<()>
    where
        T: ToMysqlValue,
    {
        if self.columns.is_empty() {
            return Ok(());
        }

        if self.result.as_mut().unwrap().is_bin {
            if self.col == 0 {
                self.result.as_mut().unwrap().writer.write_u8(0x00)?;

                // leave space for nullmap
                self.data.resize(self.bitmap_len, 0);
            }

            let c = self.columns.get(self.col).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "row has more columns than specification",
                )
            })?;
            if v.is_null() {
                if c.colflags.contains(ColumnFlags::NOT_NULL_FLAG) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "given NULL value for NOT NULL column",
                    ));
                } else {
                    // https://web.archive.org/web/20170404144156/https://dev.mysql.com/doc/internals/en/null-bitmap.html
                    // NULL-bitmap-byte = ((field-pos + offset) / 8)
                    // NULL-bitmap-bit  = ((field-pos + offset) % 8)
                    self.data[(self.col + 2) / 8] |= 1u8 << ((self.col + 2) % 8);
                }
            } else {
                v.to_mysql_bin(&mut self.data, c)?;
            }
        } else {
            v.to_mysql_text(self.result.as_mut().unwrap().writer)?;
        }
        self.col += 1;
        Ok(())
    }

    /// Indicate that no more column data will be written for the current row.
    pub async fn end_row(&mut self) -> io::Result<()> {
        if self.columns.is_empty() {
            self.col += 1;
            return Ok(());
        }

        if self.col != self.columns.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "row has fewer columns than specification",
            ));
        }

        if self.result.as_mut().unwrap().is_bin {
            self.result
                .as_mut()
                .unwrap()
                .writer
                .write_all(&self.data[..])?;
            self.data.clear();
        }
        self.result.as_mut().unwrap().writer.end_packet().await?;
        self.col = 0;

        Ok(())
    }

    /// Write a single row as a part of this resultset.
    ///
    /// Note that the row *must* conform to the column specification provided to
    /// [`QueryResultWriter::start`](struct.QueryResultWriter.html#method.start). If it does not,
    /// this method will return an error indicating that an invalid value type or specification was
    /// provided.
    pub async fn write_row<I, E>(&mut self, row: I) -> io::Result<()>
    where
        I: IntoIterator<Item = E>,
        E: ToMysqlValue,
    {
        if !self.columns.is_empty() {
            for v in row {
                self.write_col(v)?;
            }
        }
        self.end_row().await
    }
}

impl<'a, W: AsyncWrite + Unpin + 'a> RowWriter<'a, W> {
    async fn finish_inner(&mut self, extra_info: &str, complete: bool) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }

        self.finished = true;

        if !self.columns.is_empty() && self.col != 0 {
            self.end_row().await?;
        }

        if complete {
            if self.columns.is_empty() {
                // response to no column query is always an OK packet
                let resp = OkResponse {
                    info: extra_info.to_string(),
                    ..Default::default()
                };
                self.result.as_mut().unwrap().last_end = Some(Finalizer::Ok(resp));
            } else if self
                .client_capabilities
                .contains(CapabilityFlags::CLIENT_DEPRECATE_EOF)
            {
                // response to no column query is always an OK packet
                let resp = OkResponse {
                    info: extra_info.to_string(),
                    header: 0xfe,
                    ..Default::default()
                };
                self.result.as_mut().unwrap().last_end = Some(Finalizer::Ok(resp));
            } else {
                // we wrote out at least one row
                self.result.as_mut().unwrap().last_end = Some(Finalizer::Eof);
            }
        }

        Ok(())
    }

    /// Indicate to the client that no more rows are coming.
    pub async fn finish(self) -> io::Result<()> {
        self.finish_with_info("").await
    }

    /// End this resultset response, and indicate to the client that no more rows are coming.
    pub async fn finish_one(self) -> io::Result<QueryResultWriter<'a, W>> {
        self.finish_one_with_info("").await
    }

    /// Indicate to the client that no more rows are coming.
    pub async fn finish_with_info(self, extra_info: &str) -> io::Result<()> {
        self.finish_one_with_info(extra_info)
            .await?
            .no_more_results()
            .await
    }

    /// End this resultset response, and indicate to the client that no more rows are coming.
    pub async fn finish_one_with_info(
        mut self,
        extra_info: &str,
    ) -> io::Result<QueryResultWriter<'a, W>> {
        self.finish_inner(extra_info, true).await?;

        // we know that dropping self will see self.finished == true,
        // and so Drop won't try to use self.result.
        Ok(self.result.take().unwrap())
    }

    /// Finish the prepared CALL OUT/INOUT result set.
    ///
    /// Metadata carries the OUT marker; the row terminator keeps the result chain open.
    pub async fn finish_out_params(mut self) -> io::Result<QueryResultWriter<'a, W>> {
        self.finish_inner("", true).await?;
        let mut result = self.result.take().unwrap();
        result.finalize(true).await?;
        Ok(result)
    }

    /// End this resultset response, and indicate to the client there was an error.
    pub async fn finish_error<E>(mut self, kind: ErrorKind, msg: &E) -> io::Result<()>
    where
        E: Borrow<[u8]>,
    {
        self.finish_inner("", false).await?;

        self.result.take().unwrap().error(kind, msg).await
    }
}

#[cfg(test)]
mod tests {
    use byteorder::{ByteOrder, LittleEndian};
    use mysql_common::constants::{CapabilityFlags, ColumnFlags, ColumnType, StatusFlags};
    use tokio::io::{duplex, AsyncReadExt};

    use super::QueryResultWriter;
    use crate::packet_writer::PacketWriter;
    use crate::{Column, OkResponse};

    #[tokio::test]
    async fn multiple_results_mark_only_intermediate_terminator() {
        let (stream, mut reader) = duplex(4096);
        {
            let mut packets = PacketWriter::new(stream);
            let columns = [Column {
                table: String::new(),
                column: "value".to_string(),
                coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
                colflags: ColumnFlags::empty(),
            }];
            let result = QueryResultWriter::new(
                &mut packets,
                false,
                CapabilityFlags::CLIENT_DEPRECATE_EOF | CapabilityFlags::CLIENT_PROTOCOL_41,
            );
            let mut rows = result.start(&columns).await.unwrap();
            rows.write_row([Some(b"one".as_slice())]).await.unwrap();
            let result = rows.finish_one().await.unwrap();
            result.completed(OkResponse::default()).await.unwrap();
        }

        let mut wire = Vec::new();
        reader.read_to_end(&mut wire).await.unwrap();
        let mut offset = 0;
        let mut statuses = Vec::new();
        while offset + 4 <= wire.len() {
            let payload_len = LittleEndian::read_u24(&wire[offset..offset + 3]) as usize;
            let payload_start = offset + 4;
            let payload_end = payload_start + payload_len;
            assert!(payload_end <= wire.len());
            let payload = &wire[payload_start..payload_end];
            if payload.len() >= 7
                && (payload[0] == 0xfe || (payload_end == wire.len() && payload[0] == 0x00))
            {
                statuses.push(LittleEndian::read_u16(&payload[3..5]));
            }
            offset = payload_end;
        }

        assert_eq!(statuses.len(), 2);
        assert_ne!(
            statuses[0] & StatusFlags::SERVER_MORE_RESULTS_EXISTS.bits(),
            0
        );
        assert_eq!(
            statuses[1] & StatusFlags::SERVER_MORE_RESULTS_EXISTS.bits(),
            0
        );
    }

    #[tokio::test]
    async fn prepared_out_parameters_mark_metadata_and_keep_final_status_reachable() {
        let (stream, mut reader) = duplex(4096);
        {
            let mut packets = PacketWriter::new(stream);
            let columns = [Column {
                table: String::new(),
                column: "p_output".to_string(),
                coltype: ColumnType::MYSQL_TYPE_VAR_STRING,
                colflags: ColumnFlags::empty(),
            }];
            let result = QueryResultWriter::new(
                &mut packets,
                true,
                CapabilityFlags::CLIENT_DEPRECATE_EOF
                    | CapabilityFlags::CLIENT_PROTOCOL_41
                    | CapabilityFlags::CLIENT_PS_MULTI_RESULTS,
            );
            let mut rows = result.start_out_params(&columns).await.unwrap();
            rows.write_row([Some(b"output".as_slice())]).await.unwrap();
            let result = rows.finish_out_params().await.unwrap();
            result.completed(OkResponse::default()).await.unwrap();
        }

        let mut wire = Vec::new();
        reader.read_to_end(&mut wire).await.unwrap();
        let mut offset = 0;
        let mut statuses = Vec::new();
        while offset + 4 <= wire.len() {
            let payload_len = LittleEndian::read_u24(&wire[offset..offset + 3]) as usize;
            let payload_start = offset + 4;
            let payload_end = payload_start + payload_len;
            assert!(payload_end <= wire.len());
            let payload = &wire[payload_start..payload_end];
            if payload.len() >= 7
                && (payload[0] == 0xfe || (payload_end == wire.len() && payload[0] == 0x00))
            {
                statuses.push(LittleEndian::read_u16(&payload[3..5]));
            }
            offset = payload_end;
        }

        assert_eq!(statuses.len(), 3);
        assert_ne!(statuses[0] & StatusFlags::SERVER_PS_OUT_PARAMS.bits(), 0);
        assert_ne!(
            statuses[0] & StatusFlags::SERVER_MORE_RESULTS_EXISTS.bits(),
            0
        );
        assert_eq!(
            statuses[1] & StatusFlags::SERVER_MORE_RESULTS_EXISTS.bits(),
            StatusFlags::SERVER_MORE_RESULTS_EXISTS.bits()
        );
        assert_eq!(statuses[1] & StatusFlags::SERVER_PS_OUT_PARAMS.bits(), 0);
        assert_eq!(statuses[2], 0);
    }
}
